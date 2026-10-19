//! PLAN-INDEP of regions (`docs/design/kernel-regions.md` §3.3): a run
//! with regions on and the same run with them off, through the layout bit
//! and with everything else equal, make the same run: the same end and
//! charge, the same expanded charge program on the tape, the same prints,
//! parks, requests and effect ids, and at every slice return and park the
//! same sealed semantic checkpoint, with a checkpoint sealed under either
//! plan resuming under the other. Every case runs so with charge Exact and
//! with charge Off, whose slices count the run's pace (FIG-6775).

use std::sync::Arc;

use lash_kernel_doc::{Document, Name, parse_document, print_document};
use lash_kernel_exec::charge::Weight;
use lash_kernel_vm::kernel_internals::{
    Layout, RegionWork, call_islands, in_charge, loop_regions, prepare_with_layout, transient_hold,
};
use lash_kernel_vm::{Charge, Machine, PreparedLibrary, Program};

use crate::machine::{Run, case_program, run_program_in};
use crate::projection::{Image, Plan, Projector, corpus_agrees, taped_work};
use crate::{Case, Environment, Expected, ExpectedEnd, HarnessError, Observations};

/// Regions on: the natural layout.
const ON: u64 = 0;
/// Regions off, and everything else equal.
const OFF: u64 = Layout(ON).without_regions().0;
/// The slices every case runs under, the last unbounded.
const SLICES: [u64; 7] = [1, 2, 3, 7, 50, 333, u64::MAX];
/// The charge policies every case runs under: under Off, charged code
/// runs paced, and the slices count its pace.
const CHARGES: [Charge; 2] = [Charge::Exact, Charge::Off];
/// The most bounds a case's sweep runs: its first and last events' and an
/// even spread between.
const BOUNDS: usize = 240;

/// Programs whose loops form regions, each in a function: the regions
/// law's own, since corpus loops are barriers or session code. Each names
/// what it reaches.
const PROGRAMS: &[(&str, &str, u32)] = &[
    (
        "an integer sum loop",
        "fn sum(n) { let i = 0 let s = 0 while num.lt(i, n) { set s = num.add(s, i) set i = num.add(i, 1) } return s }\n\
         main { let r = call sum(40) finish r }",
        16,
    ),
    (
        "a float step loop",
        "fn step(n) { let i = 0 let x = 0.5 while num.lt(i, n) { set x = num.add(x, 0.25) set i = num.add(i, 1) } return x }\n\
         main { let r = call step(30) finish r }",
        16,
    ),
    (
        "an integer overflowing into a big integer inside the batch",
        "fn grow(n) { let i = 0 let x = 3 while num.lt(i, n) { set x = num.mul(x, 7) set i = num.add(i, 1) } return x }\n\
         main { let r = call grow(30) finish r }",
        16,
    ),
    (
        "a raise inside the batch, caught",
        "fn down(n) { let q = 0 let j = n while num.lt(0, 1) { set q = num.div(60, j) set j = num.sub(j, 1) } return q }\n\
         main { try { let r = call down(6) finish r } catch e { finish e.kind } }",
        16,
    ),
    (
        "a slot that changes kind",
        "fn kinds(n) { let i = 0 let x = null while num.lt(i, n) { set x = num.lt(i, 3) set i = num.add(i, 1) } return x }\n\
         main { let r = call kinds(12) finish r }",
        16,
    ),
    (
        "nested loops",
        "fn grid(n) { let i = 0 let s = 0 while num.lt(i, n) { set i = num.add(i, 1) let j = 0 while num.lt(j, i) { set s = num.add(s, j) set j = num.add(j, 1) } } return s }\n\
         main { let r = call grid(9) finish r }",
        16,
    ),
    (
        "a loop in recursion past the call depth",
        "fn deep(n) { if num.lt(n, 1) { return 0 } let i = 0 while num.lt(i, 2) { set i = num.add(i, 1) } let r = call deep(num.sub(n, 1)) return num.add(r, i) }\n\
         main { let r = call deep(12) finish r }",
        6,
    ),
    (
        "if and else in the body",
        "fn branch(n) { let i = 0 let s = 0 while num.lt(i, n) { if num.lt(num.rem_trunc(i, 3), 1) { set s = num.add(s, i) } else { set s = num.sub(s, i) } set i = num.add(i, 1) } return s }\n\
         main { let r = call branch(14) finish r }",
        16,
    ),
    (
        "continue and break in the body",
        "fn skip(n) { let i = 0 let s = 0 while true { set i = num.add(i, 1) if num.lt(n, i) { break } if num.lt(num.rem_trunc(i, 4), 1) { continue } set s = num.add(s, i) } return s }\n\
         main { let r = call skip(13) finish r }",
        16,
    ),
    (
        "a return inside the loop",
        "fn find(n) { let i = 0 while num.lt(i, n) { if num.lt(9, i) { return i } set i = num.add(i, 1) } return 0 }\n\
         main { let r = call find(20) finish r }",
        16,
    ),
    (
        "a for over a list whose elements are not all scalars",
        "fn mixed(xs) { let s = 0 for x in xs { set s = num.add(s, x) } return s }\n\
         main { try { let r = call mixed([1, 2, 3.5, 4, \"a\", 6]) finish r } catch e { finish e.kind } }",
        16,
    ),
    (
        "loops with barriers between them",
        "fn mixed(n) { let i = 0 let s = 0 while num.lt(i, n) { set s = num.add(s, 2) set i = num.add(i, 1) } print s while num.lt(i, num.add(n, 3)) { print i set i = num.add(i, 1) } let t = 0 while num.lt(t, n) { set t = num.add(t, 1) } return (s, i, t) }\n\
         main { let r = call mixed(10) finish r }",
        16,
    ),
];

/// Every shard exercises these R1/R2 mutation witnesses: nested arm exits
/// and ended declarations, a for cursor with continue, and a declaration
/// followed by break. Their full sweeps keep each shard red-capable even
/// when its assigned corpus cases and smith seeds form no such regions.
const EXIT_PROGRAMS: &[(&str, &str, u32)] = &[
    (
        "nested arms and a let in an arm",
        "fn arms(n) { let i = 0 let s = 0 while num.lt(i, n) { if num.lt(i, 6) { let d = num.mul(i, 2) if num.lt(d, 7) { set s = num.add(s, d) } else { set s = num.sub(s, 1) } } else { set s = num.add(s, 1) } set i = num.add(i, 1) } return s }\n\
         main { let r = call arms(13) finish r }",
        16,
    ),
    (
        "a for over a list, with continue",
        "fn total(xs) { let s = 0 for x in xs { if num.lt(x, 3) { continue } set s = num.add(s, x) } return s }\n\
         main { let r = call total([1, 2, 3, 4, 5, 6, 7, 8, 9]) finish r }",
        16,
    ),
    (
        "a for over a tuple, with a let and a break",
        "fn first(xs) { let s = 0 for x in xs { let y = num.mul(x, x) if num.lt(30, y) { break } set s = num.add(s, y) } return s }\n\
         main { let r = call first((1, 2, 3, 4, 5, 6, 7)) finish r }",
        16,
    ),
];

/// Programs whose calls run as islands (`docs/design/kernel-regions.md`
/// §5 R4), each named for what it reaches.
const ISLANDS: &[(&str, &str, u32)] = &[
    (
        "declared recursion",
        "fn fib(n) { if num.lt(n, 2) { return n } return num.add(call fib(num.sub(n, 1)), call fib(num.sub(n, 2))) }\n\
         main { let r = call fib(7) finish r }",
        64,
    ),
    (
        "closure recursion through its captured self",
        "fn outer(k) { let f = absent set f = fn(n) { if num.lt(n, 2) { return n } return num.add(apply f(num.sub(n, 1)), apply f(num.sub(n, 2))) } let r = apply f(k) return r }\n\
         main { let r = call outer(7) finish r }",
        64,
    ),
    (
        "mutual recursion",
        "fn even(n) { if num.lt(n, 1) { return true } return call odd(num.sub(n, 1)) }\n\
         fn odd(n) { if num.lt(n, 1) { return false } return call even(num.sub(n, 1)) }\n\
         main { let r = call even(17) finish r }",
        64,
    ),
    (
        "a depth bound crossed inside recursion",
        "fn down(n) { if num.lt(n, 1) { return 0 } return num.add(call down(num.sub(n, 1)), 1) }\n\
         main { let r = call down(40) finish r }",
        24,
    ),
    (
        "a raise deep in recursion, caught",
        "fn f(n) { if num.lt(n, 1) { return num.div(1, n) } return num.add(call f(num.sub(n, 1)), 1) }\n\
         main { try { let r = call f(9) finish r } catch e { finish e.kind } }",
        64,
    ),
    (
        "an integer overflowing into a big integer after a call returns",
        "fn fact(n) { if num.lt(n, 2) { return 1 } return num.mul(n, call fact(num.sub(n, 1))) }\n\
         main { let r = call fact(23) finish r }",
        64,
    ),
    (
        "variables, writes and nested blocks in island bodies",
        "fn g(n) { let a = num.add(n, 1) set a = num.mul(a, 2) if num.lt(a, 12) { let b = num.add(a, 3) if num.lt(b, 9) { return b } set a = b } return num.sub(a, 1) }\n\
         fn h(n) { if num.lt(n, 1) { return 0 } return num.add(call g(n), call h(num.sub(n, 1))) }\n\
         main { let r = call h(5) finish r }",
        64,
    ),
    (
        "a body that falls off its end, and helper calls",
        "fn p(n) { if num.lt(n, 0) { return 1 } }\n\
         fn q(n) { if num.lt(n, 1) { return 0 } let x = num.add(invoke num.add(n, 1), 0) return invoke num.add(call q(num.sub(n, 1)), num.sub(x, n)) }\n\
         fn r(n) { if num.lt(n, 1) { return call p(3) } return call r(num.sub(n, 1)) }\n\
         main { let a = call q(5) let c = call r(4) finish (a, c) }",
        64,
    ),
    (
        "an island root nested in an expression, and an island with a print outside it",
        "fn fib(n) { if num.lt(n, 2) { return n } return num.add(call fib(num.sub(n, 1)), call fib(num.sub(n, 2))) }\n\
         fn w(n) { print n let r = num.add(call fib(n), 1) return r }\n\
         main { let r = call w(6) finish r }",
        64,
    ),
    (
        "texts, lists and records an island holds, passed, bound and returned, and discarded calls",
        "fn echo(v) { return v }\n\
         fn tag(n) { if num.lt(n, 0) { return \"negative\" } do call echo(n) return \"done\" }\n\
         fn pass(xs, t) { let u = xs do call echo(t) do u if same(call echo(u), t) { return t } return call echo(call echo(u)) }\n\
         main { let a = call tag(-1) let b = call tag(2) let c = call pass([1, 2], \"x\") let d = call pass({k: \"v\"}, 3) let e = call echo(\"short\") let f = call pass(\"y\", \"y\") finish (a, b, c, d, e, f) }",
        64,
    ),
    (
        "a value no island frame holds, from a capture and as an argument, left to the reference",
        "fn outer(s) { let f = fn(n) { let t = s if num.lt(n, 1) { return t } return n } let a = apply f(0) let b = apply f(1) let c = call echo(s) return (a, b, c) }\n\
         fn echo(v) { return v }\n\
         main { let r = call outer(\"a text longer than fourteen bytes\") finish r }",
        64,
    ),
    (
        "operators on held texts and closures, charged their measured formulas, and a decline",
        "fn ident(t) { return key.identity(3, absent, t) }\n\
         fn check(v) { return collection.function_check(v) }\n\
         fn ints(x) { return collection.int_check(x) }\n\
         main { let f = fn(n) { return n } let a = call ident(\"tag\") let b = call check(f) let g = apply b(5) let c = call ints(4) try { let d = call ints(\"four\") finish (a, g, c, d) } catch e { finish (a, g, c, e.kind) } }",
        64,
    ),
    (
        "callbacks a helper's body applies by root call actions, one raising mid-call",
        "main { let f = fn(x) { if num.lt(x, 3) { return num.mul(x, 2) } return num.div(12, num.sub(x, 4)) } let g = fn(x) { return num.lt(1, x) } try { let a = invoke collection.map([1, 2, 3, 5, 6], f) let b = invoke collection.filter(a, g) let c = invoke collection.map([1, 4], f) finish (a, b, c) } catch e { finish e.kind } }",
        64,
    ),
    (
        "callbacks reading their arguments' lengths and items, past an end and through a cold arm",
        "main { let f = fn(args) { return let(t = if(num.lt(list.len(args), 1), [0], args), num.mul(t[0], 2)) } let g = fn(t) { return t[2] } try { let a = invoke collection.map([[1], [2, 3], [], [4]], f) let b = invoke collection.map([(1, 2, 3), (4, 5, 6)], g) let c = invoke collection.map([[1, 2]], g) finish (a, b, c) } catch e { finish e.kind } }",
        64,
    ),
    (
        "top-level recursion through its session binding, read again after a rebinding",
        "main { let k = 1 let f = absent set f = fn(n) { if num.lt(n, 2) { return num.add(n, k) } return num.add(apply f(num.sub(n, 1)), apply f(num.sub(n, 2))) } let a = apply f(6) set k = 10 let b = apply f(6) finish (a, b) }",
        64,
    ),
    (
        "a session binding no island frame holds, and one not yet set when an island raises on it, set again between entries",
        "main { let t = (1, 2) let g = absent set g = fn(n) { if num.lt(n, 1) { return t } return apply g(num.sub(n, 1)) } let a = apply g(3) let late = absent let h = absent set h = fn(n) { if num.lt(n, 1) { return num.add(late, 1) } return apply h(num.sub(n, 1)) } try { let b = apply h(3) finish (a, b) } catch e { set late = 0 let c = apply h(3) finish (a, e.kind, c) } }",
        64,
    ),
];

/// Programs whose loop regions allocate (§5, R5): they build records,
/// lists, maps and tuples, read members through property caches, write
/// members and call natives. Each also runs with memory off and under a
/// memory bound its run passes.
const ALLOCATING: &[(&str, &str)] = &[
    (
        "records, lists and tuples built and read in the body",
        "fn rows(n) { let i = 0 let s = 0 while num.lt(i, n) { let r = {index: i, pair: [i, num.add(i, 1)], both: (i, 2)} set s = num.add(s, num.add(r.index, r.pair[1])) set i = num.add(i, 1) } return s }\n\
         main { let r = call rows(10) finish r }",
    ),
    (
        "a list kept by appends, its rows labelled by text natives",
        "fn churn(n) { let i = 0 let kept = [] while num.lt(i, n) { let scratch = {index: i, pair: [i, num.add(i, 1)], label: text.concat(\"row-\", num.to_text(i))} if num.lt(num.rem_trunc(i, 4), 1) { set kept[list.len(kept)] = scratch } set i = num.add(i, 1) } return kept }\n\
         main { let r = call churn(14) finish r }",
    ),
    (
        "a map counted by key",
        "fn tally(n) { let i = 0 let counts = map{} while num.lt(i, n) { let key = text.concat(\"key_\", num.to_text(num.rem_trunc(i, 6))) if map.contains(counts, key) { set counts[key] = num.add(map.get(counts, key), 1) } else { set counts[key] = 1 } set i = num.add(i, 1) } return counts }\n\
         main { let r = call tally(16) finish r }",
    ),
    // FIG-6820: the region's own membership tests of maps, sets and
    // records, by key kind: tuple, long text, normalized number and NaN
    // keys present and missing, and, after keys that answer, the keys
    // whose call raises. A long key is made in the loop from short texts:
    // a long literal there is a barrier.
    (
        "membership tests of maps, sets and records by key kind",
        "fn probe(n) { let i = 0 let s = 0 let m = map{\"key-past-limit1\": 1, (1, 2): 2, 3: 3, nan: 4} let xs = set{\"a\", 2.5, (1, ())} let r = {a: 1, b2: 2, key_past_limit2: 3} while num.lt(i, n) { let k = num.rem_trunc(i, 3) if map.contains(m, (1, num.add(k, 1))) { set s = num.add(s, 1) } if map.contains(m, text.concat(\"key-past-limit\", num.to_text(k))) { set s = num.add(s, 10) } if map.contains(m, num.add(k, 2.0)) { set s = num.add(s, 100) } if map.contains(m, nan) { set s = num.add(s, 1000) } if set.contains(xs, num.add(k, 1.5)) { set s = num.add(s, 10000) } if record.contains(r, text.concat(\"b\", num.to_text(k))) { set s = num.add(s, 100000) } if record.contains(r, text.concat(\"key_past_limit\", num.to_text(k))) { set s = num.add(s, 1000000) } set i = num.add(i, 1) } return s }\n\
         main { let r = call probe(7) finish r }",
    ),
    (
        "a map tested at an unhashable key",
        "fn probe(m, keys) { let s = 0 for key in keys { if map.contains(m, key) { set s = num.add(s, 1) } } return s }\n\
         main { try { let r = call probe(map{1: 1}, [1, 2, 1, [1]]) finish r } catch e { finish e.kind } }",
    ),
    (
        "a record tested at a number",
        "fn probe(r, keys) { let s = 0 for key in keys { if record.contains(r, key) { set s = num.add(s, 1) } } return s }\n\
         main { try { let r = call probe({a: 1}, [\"a\", \"b\", \"a\", 1]) finish r } catch e { finish e.kind } }",
    ),
    (
        "a record whose shape changes under its field reads and writes",
        "fn shapes(n) { let i = 0 let s = 0 while num.lt(i, n) { let r = {a: i, b: 1} if num.lt(num.rem_trunc(i, 2), 1) { set r = {b: 2, c: 3, a: i} } set s = num.add(s, r.a) set r.b = s set i = num.add(i, 1) } return s }\n\
         main { let r = call shapes(10) finish r }",
    ),
    (
        "a for over a list of records, writing a list's members",
        "fn marks(xs) { let out = [0, 0, 0] for x in xs { set out[num.rem_trunc(x.k, 3)] = x.v } return out }\n\
         main { try { let r = call marks([{k: 1, v: 2}, {k: 2, v: 3}, {k: 4, v: [5]}, {k: 3, v: \"a\"}, {k: 5}, {k: 6, v: 7}]) finish r } catch e { finish e.kind } }",
    ),
    (
        "an index past a list's end, raised and caught",
        "fn past(xs) { let s = 0 let i = 0 while num.lt(i, 6) { set s = num.add(s, xs[i]) set i = num.add(i, 1) } return s }\n\
         main { try { let r = call past([1, 2, 3, 4]) finish r } catch e { finish e.kind } }",
    ),
];

/// FIG-6795: a linear text accumulator, whose concatenations take the
/// storage of the text the one before made; and the aliased-prefix
/// control, whose accumulator's first text is also another variable's,
/// and whose second loop keeps every older version in a variable before
/// it keeps it in a list.
const ACCUMULATORS: &[(&str, &str)] = &[
    (
        "a text accumulated by concatenation, no older version of it observed",
        "fn build(n) { let i = 0 let s = text.concat(\"a text past the inline\", \" limit\") while num.lt(i, n) { set s = text.concat(s, text.concat(\"-row-\", num.to_text(i))) set i = num.add(i, 1) } return s }\n\
         main { let r = call build(12) finish r }",
    ),
    (
        "a text accumulated over a prefix another variable holds, then with its older versions kept",
        "fn build(n) { let i = 0 let start = text.concat(\"a prefix past the inline\", \" limit\") let s = start while num.lt(i, n) { set s = text.concat(s, num.to_text(i)) set i = num.add(i, 1) } let kept = [] let prev = s let j = 0 while num.lt(j, n) { set prev = s set s = text.concat(s, num.to_text(j)) set kept[list.len(kept)] = prev set j = num.add(j, 1) } return (start, s, kept) }\n\
         main { let r = call build(9) finish r }",
    ),
];

/// Top-level programs (`docs/design/kernel-regions.md` §6): their
/// variables are session bindings, which a region holds as slots between
/// boundaries. Each names what it reaches, and whether it parks on a
/// `sleep` the run delivers. The first two run on every shard: the cold
/// sum shape's binding grows past its holding, and the park stands
/// between two regions over the same bindings, so each shard catches a
/// region that skips a session admission, publishes nothing, or holds its
/// writes past a boundary.
const SESSIONS: &[(&str, &str, bool)] = &[
    (
        "the cold sum shape at top level",
        "main { let total = 0 let i = 0 while num.lt(i, 40) { set total = num.add(total, i) set i = num.add(i, 1) } finish total }",
        false,
    ),
    (
        "a park between two top-level loops",
        "main { let i = 0 let s = 0 while num.lt(i, 10) { set s = num.add(s, i) set i = num.add(i, 1) } do sleep 0 while num.lt(i, 20) { set s = num.sub(s, i) set i = num.add(i, 1) } finish (s, i) }",
        true,
    ),
    (
        "the cold branch shape at top level",
        "main { let total = 0 let i = 0 while num.lt(i, 40) { if num.lt(num.rem_trunc(i, 2), 1) { set total = num.add(total, i) } else { set total = num.sub(total, i) } set i = num.add(i, 1) } finish total }",
        false,
    ),
    (
        "a session alias of a list, written through and rebound",
        "main { let xs = [0, 0, 0] let ys = xs let zs = 0 let i = 0 while num.lt(i, 12) { set ys[num.rem_trunc(i, 3)] = i set zs = ys set i = num.add(i, 1) } finish (xs, zs) }",
        false,
    ),
    (
        "a session binding that shrinks and grows by turns",
        "main { let x = 1 let i = 0 while num.lt(i, 20) { if num.lt(x, 2) { set x = 100000000 } else { set x = 1 } set i = num.add(i, 1) } finish x }",
        false,
    ),
    (
        "a top-level loop and a function's loop over the same names",
        "fn inner(n) { let i = 0 let s = 0 while num.lt(i, n) { set s = num.add(s, i) set i = num.add(i, 1) } return s }\n\
         main { let s = 0 let i = 0 while num.lt(i, 8) { set s = num.add(s, i) set i = num.add(i, 1) } let t = call inner(i) finish (s, t) }",
        false,
    ),
];

/// How many [`SESSIONS`] programs every shard runs.
const SESSION_WITNESSES: usize = 2;

/// The memory bound an allocating program runs under besides the
/// default: past a region's nursery, short of what the longest run holds.
const TIGHT: u64 = 96 * 1024;

/// Independent libtest laws, one per Buck shard of the target, so no law
/// carries more than a sixteenth of the program families.
const SHARDS: usize = 16;

/// Deals one law's programs to the [`SHARDS`] laws in a single sequence
/// across every family, so a family shorter than [`SHARDS`] starts where
/// the one before it stopped instead of stacking on the first laws.
struct Deal {
    shard: usize,
    next: usize,
}

impl Deal {
    /// Whether the next program in the sequence is this law's.
    fn mine(&mut self) -> bool {
        let mine = self.next % SHARDS == self.shard;
        self.next += 1;
        mine
    }
}

/// One run of a case under a plan: what it observed, its charge program,
/// its projections and the work regions committed in it.
struct Probe<'a> {
    observations: Observations,
    tape: Vec<Weight>,
    projector: Projector<'a>,
    work: RegionWork,
}

/// Runs `case` under plan `L` and the `policy`, bounded at `charge` when
/// given, sealing at every slice return and park and, with `imports`,
/// resuming the other plan's image there.
fn probe<'a, const L: u64>(
    program: &Program,
    case: &Case,
    policy: Charge,
    charge: Option<u64>,
    imports: Option<&'a [Image]>,
) -> Result<Probe<'a>, HarnessError> {
    let mut projector = Projector {
        imports,
        layout: Layout(L),
        library: Some(program.library.clone()),
        ..Projector::default()
    };
    let (observations, tape, work) = taped_work(|| {
        let mut start =
            |program, mut bounds: lash_kernel_vm::Bounds, start, source: &crate::pages::Source| {
                bounds.charge = policy;
                Plan::<L>::start(program, bounds, start, Box::new(source.clone()))
            };
        let run = Run {
            start: &mut start,
            printed: &mut || {},
            charge,
        };
        run_program_in::<Plan<L>>(
            program.clone(),
            case,
            run,
            &mut |at| Ok(at.machine),
            Some(&mut projector),
        )
    });
    Ok(Probe {
        observations: observations?,
        tape,
        projector,
        work,
    })
}

/// Whether run `on`, with regions on, and run `off`, the reference, agree:
/// the run, its checkpoints and its charge program. A memory refusal's
/// point may depend on the plan within the transient slack (§3.5): where
/// the two runs end apart and one refused memory, the one that refused
/// first charged no more than the other, its charge program is the
/// other's up to its end, their checkpoints, prints and trace agree that
/// far, and its held bytes there are within [`transient_hold`] of the
/// other plan's at that charge, which `held_at(on, charge)` gives: the
/// memory counter at the end of plan `on`'s run bounded there.
fn agree(
    on: &Probe<'_>,
    off: &Probe<'_>,
    held_at: &mut dyn FnMut(bool, u64) -> Option<u64>,
) -> bool {
    let memory = |probe: &Probe<'_>| matches!(&probe.observations.end, ExpectedEnd::Bound { name, .. } if name == "memory");
    let tapes = (
        Weight::expand(on.tape.iter().copied()),
        Weight::expand(off.tape.iter().copied()),
    );
    let prefix = corpus_agrees(
        &on.observations,
        &off.observations,
        &on.projector,
        &off.projector,
    );
    if !memory(on) && !memory(off) {
        return prefix && tapes.0 == tapes.1;
    }
    if on.observations == off.observations && tapes.0 == tapes.1 {
        return prefix;
    }
    // The run that refused first, and whether it is the run with regions.
    let first_on = match (memory(on), memory(off)) {
        (true, false) => true,
        (false, true) => false,
        _ => on.observations.charged <= off.observations.charged,
    };
    let (first, other, (earlier, later)) = if first_on {
        (on, off, (&tapes.0, &tapes.1))
    } else {
        (off, on, (&tapes.1, &tapes.0))
    };
    let (Some(charged), Some(past), Some(held)) = (
        first.observations.charged,
        other.observations.charged,
        first.projector.held,
    ) else {
        return false;
    };
    prefix
        && charged <= past
        && later.get(..earlier.len()) == Some(earlier.as_slice())
        && held_at(!first_on, charged).is_some_and(|there| held.abs_diff(there) <= transient_hold())
}

/// The bounds a case sweeps: around each event of its charge program, up
/// to `BOUNDS` of them, and past its whole charge.
fn bounds(program: &[Weight], limit: u64) -> Vec<u64> {
    let all = super::charge::sweep(program, limit);
    let mut chosen: Vec<u64> = if all.len() <= BOUNDS {
        all
    } else {
        let edge = BOUNDS / 3;
        let stride = (all.len() - 2 * edge) / edge + 1;
        let middle = all[edge..all.len() - edge].iter().step_by(stride);
        all[..edge]
            .iter()
            .chain(middle)
            .chain(&all[all.len() - edge..])
            .copied()
            .collect()
    };
    let whole = program.iter().map(|weight| weight.units()).sum::<u64>();
    if whole < limit {
        chosen.push(whole + 1);
    }
    chosen.sort_unstable();
    chosen.dedup();
    chosen
}

/// How many runs a case made under each plan.
#[derive(Default)]
struct Counts {
    cases: usize,
    runs: usize,
    bounds: usize,
    slices: usize,
    resumes: usize,
    slice_bounds: usize,
    smith_schedules: usize,
    smith_bounds: usize,
    /// Cases whose natural run with regions on committed region work.
    ran: usize,
    /// The work their natural runs committed.
    work: RegionWork,
}

impl Counts {
    fn report(&self, shard: usize, family: &str, began: std::time::Instant) {
        println!(
            "regions shard={shard} through={family}: cases={} ran_regions={} region_iterations={} island_calls={} plan_passes={} pairs={} bounds={} slices={} cross_resumes={} slice_bounds={} smith_schedules={} smith_bounds={} seconds={:.3}",
            self.cases,
            self.ran,
            self.work.iterations,
            self.work.calls,
            self.work.passes,
            self.runs,
            self.bounds,
            self.slices,
            self.resumes,
            self.slice_bounds,
            self.smith_schedules,
            self.smith_bounds,
            began.elapsed().as_secs_f64(),
        );
    }
}

/// Adds a disagreement of run `a`, with regions on, and run `b`, the
/// reference, of `case` to `failures`; `held_at` is [`agree`]'s.
fn compare(
    case: &Case,
    what: &str,
    a: Result<&Probe<'_>, &HarnessError>,
    b: Result<&Probe<'_>, &HarnessError>,
    held_at: &mut dyn FnMut(bool, u64) -> Option<u64>,
    counts: &mut Counts,
    failures: &mut Vec<String>,
) {
    counts.runs += 1;
    match (a, b) {
        (Ok(a), Ok(b)) if agree(a, b, held_at) => {}
        // The harness refuses both runs alike: a seal of a run past its
        // memory bound, which neither plan can write.
        (Err(a), Err(b)) if a.0 == b.0 => {}
        (Ok(a), Ok(b)) => failures.push(format!(
            "{}, {what}: {:?} after {} charges, {:?} after {}",
            case.name,
            a.observations,
            a.tape.len(),
            b.observations,
            b.tape.len()
        )),
        (a, b) => failures.push(format!(
            "{}, {what}: {:?}, {:?}",
            case.name,
            a.err(),
            b.err()
        )),
    }
}

/// Checks `case` under both plans across the sweep, with charge Exact and
/// Off, adding each disagreement to `failures`. Gives the work regions
/// committed in its natural run with regions on and charge Exact: where
/// that run ended an iteration in a region or entered an island, its run
/// with charge Off, paced, does too.
fn check(
    on: &PreparedLibrary,
    off: &PreparedLibrary,
    case: &Case,
    counts: &mut Counts,
    failures: &mut Vec<String>,
) -> RegionWork {
    let charged = check_in(Charge::Exact, on, off, case, counts, failures);
    // A charge-bound refusal is the charge oracle's: Off has no bound.
    if matches!(&case.expected.end, ExpectedEnd::Bound { name, .. } if name == "charge") {
        return charged;
    }
    let paced = check_in(Charge::Off, on, off, case, counts, failures);
    if (charged.iterations > 0 && paced.iterations == 0) || (charged.calls > 0 && paced.calls == 0)
    {
        failures.push(format!(
            "{}: regions committed {charged:?} with charge Exact and {paced:?} with charge Off",
            case.name
        ));
    }
    charged
}

/// Checks `case` under both plans and the `policy` across the sweep, and
/// gives the work regions committed in its natural run with regions on,
/// which its unsliced run after the sweep commits again: a plan the sweep
/// retired would leave that run, and every one after it, to the
/// reference. Under Off, which has no charge bound, every slice: where its
/// pace returns a slice, and the images there, are what a plan can get
/// wrong. The images resume across plans under Exact.
fn check_in(
    policy: Charge,
    on: &PreparedLibrary,
    off: &PreparedLibrary,
    case: &Case,
    counts: &mut Counts,
    failures: &mut Vec<String>,
) -> RegionWork {
    let exact = policy == Charge::Exact;
    let mut case = case.clone();
    if !exact {
        case.name = format!("{} (charge off)", case.name);
    }
    case.environment.resume = false;
    case.environment.slice = u64::MAX;
    let Some(on) = case_program(on, &case) else {
        return RegionWork::default();
    };
    let off = Program {
        library: off.clone(),
        ..on.clone()
    };
    let natural = match probe::<ON>(&on, &case, policy, None, None) {
        Ok(natural) => natural,
        Err(error) => {
            failures.push(format!("{}, natural run: {error:?}", case.name));
            return RegionWork::default();
        }
    };
    if natural.observations.end == ExpectedEnd::Refused {
        return RegionWork::default();
    }
    counts.cases += 1;
    counts.work.iterations += natural.work.iterations;
    counts.work.calls += natural.work.calls;
    counts.work.passes += natural.work.passes;
    counts.ran += usize::from(natural.work != RegionWork::default());
    let program = Weight::expand(natural.tape.iter().copied());
    let whole = natural.observations.charged.unwrap_or(0);
    // The memory counter at the end of a run under either plan bounded at
    // a charge, resuming the given images.
    let held = |case: &Case, imports: Option<&[Image]>, regions: bool, charge: u64| {
        let probe = if regions {
            probe::<ON>(&on, case, policy, Some(charge), imports)
        } else {
            probe::<OFF>(&off, case, policy, Some(charge), imports)
        };
        probe.ok()?.projector.held
    };
    let swept = if exact {
        bounds(&program, case.environment.bounds.charge)
    } else {
        Vec::new()
    };
    for bound in swept {
        counts.bounds += 1;
        let a = probe::<ON>(&on, &case, policy, Some(bound), None);
        let b = probe::<OFF>(&off, &case, policy, Some(bound), None);
        compare(
            &case,
            &format!("bound {bound}"),
            a.as_ref(),
            b.as_ref(),
            &mut |regions, charge| held(&case, None, regions, charge),
            counts,
            failures,
        );
    }
    for slice in SLICES {
        counts.slices += 1;
        let mut sliced = case.clone();
        sliced.environment.slice = slice;
        sliced.environment.max_steps = sliced.environment.max_steps.max(1_000_000);
        let a = probe::<ON>(&on, &sliced, policy, None, None);
        let b = probe::<OFF>(&off, &sliced, policy, None, None);
        compare(
            &sliced,
            &format!("slice {slice}"),
            a.as_ref(),
            b.as_ref(),
            &mut |regions, charge| held(&sliced, None, regions, charge),
            counts,
            failures,
        );
        if slice == u64::MAX
            && let Ok(a) = &a
            && a.work != natural.work
        {
            failures.push(format!(
                "{}: regions committed {:?} in the natural run and {:?} in the unsliced run after the sweep",
                case.name, natural.work, a.work
            ));
        }
        // Cross-resume: each plan resumes the other's images, which the
        // comparison above found equal, so the two resumed runs are one
        // resume under either plan. A resumed run is compared with a
        // resumed run: where a resume prices its memory against a
        // continuous run is the slice law's, not the plans'.
        if let (Ok(a), Ok(b)) = (&a, &b)
            && exact
        {
            counts.resumes += 1;
            let off_resumed = probe::<OFF>(&off, &sliced, policy, None, Some(&a.projector.images));
            let on_resumed = probe::<ON>(&on, &sliced, policy, None, Some(&b.projector.images));
            let what = format!("slice {slice}, each resuming the other's images");
            compare(
                &sliced,
                &what,
                on_resumed.as_ref(),
                off_resumed.as_ref(),
                &mut |regions, charge| {
                    let images = if regions {
                        &b.projector.images
                    } else {
                        &a.projector.images
                    };
                    held(&sliced, Some(images), regions, charge)
                },
                counts,
                failures,
            );
        }
        if !exact {
            continue;
        }
        for bound in [whole / 3, whole / 2, whole.saturating_sub(5)] {
            counts.slice_bounds += 1;
            let a = probe::<ON>(&on, &sliced, policy, Some(bound), None);
            let b = probe::<OFF>(&off, &sliced, policy, Some(bound), None);
            let what = format!("slice {slice}, bound {bound}");
            compare(
                &sliced,
                &what,
                a.as_ref(),
                b.as_ref(),
                &mut |regions, charge| held(&sliced, None, regions, charge),
                counts,
                failures,
            );
        }
    }
    natural.work
}

/// Checks smith program `smith` under both plans: its own schedule, every
/// slice, and bounds around its charge, adding each disagreement to
/// `failures`.
fn check_smith(
    name: &str,
    smith: &crate::smith::Smith,
    counts: &mut Counts,
    failures: &mut Vec<String>,
) {
    if smith.admit().is_err() {
        return;
    }
    counts.cases += 1;
    let mut schedules: Vec<Vec<u64>> = vec![smith.schedule.slices.clone()];
    schedules.extend(SLICES.iter().map(|slice| vec![*slice]));
    let plans = crate::smith::Plans::<ON, OFF>::new(smith);
    let mut whole = None;
    for slices in &schedules {
        counts.smith_schedules += 1;
        counts.runs += 1;
        match plans.agree(smith, slices) {
            Ok(charged) => {
                whole.get_or_insert(charged);
            }
            Err(error) => failures.push(format!("{name}, slices {slices:?}: {error}")),
        }
    }
    let whole = whole.unwrap_or(0);
    for charge in [
        0,
        whole / 3,
        whole / 2,
        whole.saturating_sub(1),
        whole,
        whole + 1,
    ] {
        counts.smith_bounds += 1;
        let mut bounded = smith.clone();
        bounded.schedule.bounds.charge_limit = charge;
        counts.runs += 1;
        if let Err(error) = plans.agree(&bounded, &[u64::MAX]) {
            failures.push(format!("{name}, bound {charge}: {error}"));
        }
    }
}

/// `document` with `main`'s body moved into a function `main` calls, as
/// `scripts/perf/kernel/embedded.py` wraps its bodies: its variables are
/// then a frame's, not session bindings, so its loops can form regions.
fn wrap(document: &Document) -> Option<Document> {
    let mut document = document.clone();
    let name = Name::new("wrapped_main");
    if document.functions.contains_key(&name) {
        return None;
    }
    let caller =
        parse_document("kernel 1\nmain { let result = call wrapped_main() return result }")
            .ok()?
            .main;
    let body = std::mem::replace(&mut document.main, caller);
    document.functions.insert(
        name,
        lash_kernel_doc::Function {
            params: Vec::new(),
            body,
            start: None,
        },
    );
    Some(document)
}

/// `case` with its document wrapped ([`wrap`]).
fn wrapped(case: &Case) -> Option<Case> {
    let document = wrap(&parse_document(&case.document).ok()?)?;
    let mut wrapped = case.clone();
    wrapped.name = format!("{} (wrapped)", case.name);
    wrapped.document = print_document(&document);
    Some(wrapped)
}

/// The region program `text` as a case, its library functions named.
fn program_case(name: &str, text: &str, call_depth: u32) -> Case {
    let registry = super::corpus_registry();
    let mut source = String::from("kernel 1\nnumbers by_spelling\n");
    for (id, function) in registry.iter() {
        source.push_str(&format!("use {} = @{id}\n", function.definition().name));
    }
    source.push_str(text);
    let mut document =
        parse_document(&source).unwrap_or_else(|error| panic!("{name} must parse: {error}"));
    document.manifest.functions = lash_kernel_check::requirements(&document, &registry).functions;
    // A refused program runs in no plan, and the law would compare nothing.
    let mut admission = lash_kernel_check::Environment::new(&registry);
    admission.effects = document.manifest.signatures();
    lash_kernel_check::admit(&document, &admission)
        .unwrap_or_else(|refusal| panic!("{name} must be admitted: {refusal:?}"));
    let mut environment = Environment::default();
    environment.bounds.call_depth = call_depth;
    environment.max_steps = 1_000_000;
    Case {
        name: name.into(),
        document: print_document(&document),
        environment,
        expected: Expected {
            prints: Vec::new(),
            end: ExpectedEnd::Refused,
            trace: Vec::new(),
            charged: None,
            parks: None,
        },
    }
}

/// How many loop regions and call islands `case`'s document forms under
/// `library`.
fn formed(library: &PreparedLibrary, case: &Case) -> (usize, usize) {
    let document = parse_document(&case.document).expect("a region program parses");
    let program = Program {
        release: crate::pages::RELEASE,
        document: Arc::new(document),
        library: library.clone(),
    };
    (
        loop_regions(&program).expect("a region program is admitted"),
        call_islands(&program).expect("a region program is admitted"),
    )
}

/// How many loop regions and call islands `case`'s document forms under
/// `on`: the same with charge Off, whose charged code is paced, and none
/// under `off`, with either policy.
fn formed_alike(on: &PreparedLibrary, off: &PreparedLibrary, case: &Case) -> (usize, usize) {
    let formed_on = formed(on, case);
    assert_eq!(
        formed(&in_charge(on, Charge::Off), case),
        formed_on,
        "{}: charge Off forms other plans",
        case.name
    );
    for policy in CHARGES {
        assert_eq!(
            formed(&in_charge(off, policy), case),
            (0, 0),
            "{}: plans form with regions off, charge {policy:?}",
            case.name
        );
    }
    formed_on
}

/// Programs whose loop regions their natural run leaves to the reference:
/// [`ran`] requires every other formed plan to run. The dialect decline
/// witnesses raise or decline at their loop body's first statement in
/// every iteration, so no iteration ends in a region.
const LEFT_TO_THE_REFERENCE: &[&str] = &[
    "python region: kind decline",
    "python region: append to a map",
    "python region: get on a list",
    "python region: long text keys",
    "typescript region: branded record",
    "typescript region: inherited name",
];

/// Programs whose helpers' plans run a loop as a region over its cursor
/// (FIG-6780): [`ran`] requires their natural run to end a pass in one,
/// or the comparison leaves those plans' generated code against itself.
const PLAN_REGIONS: &[&str] = &[
    "typescript islands: map and filter callbacks",
    "typescript islands: a callback leaving its island on a text",
    "typescript islands: fused callbacks that capture, shorten and raise",
];

/// Adds a failure where `plans`, the loop regions and call islands
/// program `name` forms, are not met by `work`, what its natural run with
/// regions on committed: a formed plan that never runs leaves the
/// comparison to the reference against itself. Plans the program leaves
/// to the reference by [`LEFT_TO_THE_REFERENCE`] are not required.
fn ran(name: &str, plans: (usize, usize), work: RegionWork, failures: &mut Vec<String>) {
    if plans.0 > 0 && work.iterations == 0 && !LEFT_TO_THE_REFERENCE.contains(&name) {
        failures.push(format!(
            "{name}: {} loop regions form, and its natural run ended no iteration in one",
            plans.0
        ));
    }
    if PLAN_REGIONS.contains(&name) && work.passes == 0 {
        failures.push(format!(
            "{name}: its helpers' plans form loop regions, and its natural run ended no pass in one"
        ));
    }
    if plans.1 > 0 && work.calls == 0 {
        failures.push(format!(
            "{name}: {} call islands form, and its natural run entered none",
            plans.1
        ));
    }
}

/// PLAN-INDEP of regions (`docs/design/kernel-regions.md` §3.3): every
/// corpus case, the 64-seed smith programs, the region programs and the
/// call-island programs, each
/// as written and wrapped in a function, run alike with regions on and
/// off, under every bound around their charge programs' events and every
/// slice, with each plan resuming the other's checkpoints. Each but the
/// smith programs runs so with charge Off too, under every slice: its
/// plans form as under Exact, run where they run charged, and its slices
/// count its pace.
fn regions_on_and_off_agree(shard: usize) {
    let began = std::time::Instant::now();
    let registry = Arc::new(super::corpus_registry());
    let on = prepare_with_layout(Arc::clone(&registry), Layout(ON));
    let off = prepare_with_layout(Arc::clone(&registry), Layout(OFF));
    let mut counts = Counts::default();
    let mut failures = Vec::new();
    let mut regions = 0;
    let mut deal = Deal { shard, next: 0 };
    let programs = PROGRAMS.iter().filter(|_| deal.mine()).chain(EXIT_PROGRAMS);
    let mut region_programs = 0;
    for (name, text, call_depth) in programs {
        region_programs += 1;
        let case = program_case(name, text, *call_depth);
        let plans = formed_alike(&on, &off, &case);
        assert!(plans.0 > 0, "{name}: no loop region forms");
        regions += plans.0;
        let work = check(&on, &off, &case, &mut counts, &mut failures);
        ran(name, plans, work, &mut failures);
    }
    for (name, text) in ALLOCATING {
        if !deal.mine() {
            continue;
        }
        region_programs += 1;
        let case = program_case(name, text, 16);
        let plans = formed_alike(&on, &off, &case);
        assert!(plans.0 > 0, "{name}: no loop region forms");
        regions += plans.0;
        let work = check(&on, &off, &case, &mut counts, &mut failures);
        ran(name, plans, work, &mut failures);
        let mut unaccounted = case.clone();
        unaccounted.name = format!("{name} (memory off)");
        unaccounted.environment.bounds.memory_meter = lash_kernel_vm::MemoryMeter::Off;
        check(&on, &off, &unaccounted, &mut counts, &mut failures);
        let mut tight = case;
        tight.name = format!("{name} (memory {TIGHT})");
        tight.environment.bounds.memory = TIGHT;
        check(&on, &off, &tight, &mut counts, &mut failures);
    }
    for (index, (name, text, parks)) in SESSIONS.iter().enumerate() {
        if index >= SESSION_WITNESSES && !deal.mine() {
            continue;
        }
        region_programs += 1;
        let mut case = program_case(name, text, 16);
        if *parks {
            case.environment.deliveries = vec![vec![crate::Delivery {
                request: 0,
                outcome: crate::ScriptOutcome::Elapsed,
                dropped: false,
            }]];
        }
        let plans = formed_alike(&on, &off, &case);
        assert!(plans.0 > 0, "{name}: no loop region forms");
        regions += plans.0;
        let work = check(&on, &off, &case, &mut counts, &mut failures);
        ran(name, plans, work, &mut failures);
        if !*parks {
            sessions_hold(&on, &off, &case, &mut counts, &mut failures);
        }
    }
    for (lowered, program) in crate::helper_plans::top_level_region_programs() {
        if !deal.mine() {
            continue;
        }
        region_programs += 1;
        let registry = program.library.registry();
        let on = prepare_with_layout(Arc::clone(registry), Layout(ON));
        let off = prepare_with_layout(Arc::clone(registry), Layout(OFF));
        let mut case = program_case(&lowered.id, "main { return null }", 16);
        case.document = print_document(&program.document);
        let plans = formed_alike(&on, &off, &case);
        assert!(plans.0 > 0, "{}: no loop region forms", case.name);
        regions += plans.0;
        let work = check(&on, &off, &case, &mut counts, &mut failures);
        ran(&case.name, plans, work, &mut failures);
        sessions_hold(&on, &off, &case, &mut counts, &mut failures);
    }
    counts.report(shard, "region-programs", began);
    let (mut island_programs, mut islands) = (0, 0);
    for (name, text, call_depth) in ISLANDS.iter().filter(|_| deal.mine()) {
        island_programs += 1;
        let case = program_case(name, text, *call_depth);
        let plans = formed_alike(&on, &off, &case);
        assert!(plans.1 > 0, "{name}: no call island forms");
        islands += plans.1;
        let work = check(&on, &off, &case, &mut counts, &mut failures);
        ran(name, plans, work, &mut failures);
    }
    counts.report(shard, "island-programs", began);
    // FIG-6754: actual Python invokes preserve their atom/action charge,
    // occurrences and continuations, including declines and zero division;
    // FIG-6758: both dialects' heap paths and the kinds they decline.
    // Assign each extra program once, like the existing program families.
    for (lowered, program) in crate::helper_plans::dialect_region_programs() {
        if !deal.mine() {
            continue;
        }
        let registry = program.library.registry();
        let on = prepare_with_layout(Arc::clone(registry), Layout(ON));
        let off = prepare_with_layout(Arc::clone(registry), Layout(OFF));
        let mut case = program_case(&lowered.id, "main { return null }", 16);
        case.document = print_document(&program.document);
        let plans = formed_alike(&on, &off, &case);
        assert!(plans.0 > 0, "{}", case.name);
        for call_depth in [1, 2, 3, 4, 16] {
            case.environment.bounds.call_depth = call_depth;
            let work = check(&on, &off, &case, &mut counts, &mut failures);
            if call_depth == 16 {
                ran(&case.name, plans, work, &mut failures);
            }
        }
    }
    counts.report(shard, "dialect-invokes", began);
    // FIG-6759: lowered guest calls, root call actions and the callbacks a
    // helper's plan applies run as islands, under both depths around them.
    for (lowered, program) in crate::helper_plans::island_programs() {
        if !deal.mine() {
            continue;
        }
        let registry = program.library.registry();
        let on = prepare_with_layout(Arc::clone(registry), Layout(ON));
        let off = prepare_with_layout(Arc::clone(registry), Layout(OFF));
        let mut case = program_case(&lowered.id, "main { return null }", 16);
        case.document = print_document(&program.document);
        let plans = formed_alike(&on, &off, &case);
        assert!(plans.1 > 0, "{}: no call island forms", case.name);
        for call_depth in [3, 16] {
            case.environment.bounds.call_depth = call_depth;
            let work = check(&on, &off, &case, &mut counts, &mut failures);
            if call_depth == 16 {
                ran(&case.name, plans, work, &mut failures);
            }
        }
    }
    counts.report(shard, "dialect-islands", began);
    for case in super::corpus_shards().iter().flat_map(|shard| &shard.cases) {
        if deal.mine() {
            check(&on, &off, case, &mut counts, &mut failures);
            if let Some(wrapped) = wrapped(case) {
                check(&on, &off, &wrapped, &mut counts, &mut failures);
            }
        }
    }
    counts.report(shard, "corpus", began);
    for (seed, smith) in
        crate::smith::tests::seeds().filter(|(seed, _)| *seed as usize % SHARDS == shard)
    {
        check_smith(
            &format!("smith seed {seed}"),
            &smith,
            &mut counts,
            &mut failures,
        );
        if let Some(document) = wrap(&smith.document) {
            let wrapped = crate::smith::Smith {
                document,
                schedule: smith.schedule.clone(),
            };
            let name = format!("smith seed {seed} (wrapped)");
            check_smith(&name, &wrapped, &mut counts, &mut failures);
        }
    }
    counts.report(shard, "smith", began);
    assert!(
        failures.is_empty(),
        "{} of {} paired runs disagree:\n{}",
        failures.len(),
        counts.runs,
        failures.join("\n")
    );
    println!(
        "regions on and off shard={shard}: cases={} paired runs={} region programs={region_programs} loop regions={regions} island programs={island_programs} call islands={islands} seconds={:.3}",
        counts.cases,
        counts.runs,
        began.elapsed().as_secs_f64(),
    );
}

/// Session regions hold what the reference holds (§6): `case`'s run has
/// the memory counter where the reference has it at every slice return
/// and at its end, with memory accounted and off.
/// PLAN-INDEP leaves memory out, so this is what sees a region skip a
/// session write's admission.
fn sessions_hold(
    on: &PreparedLibrary,
    off: &PreparedLibrary,
    case: &Case,
    counts: &mut Counts,
    failures: &mut Vec<String>,
) {
    for memory_meter in [
        lash_kernel_vm::MemoryMeter::On,
        lash_kernel_vm::MemoryMeter::Off,
    ] {
        let mut case = case.clone();
        case.environment.bounds.memory_meter = memory_meter;
        let Some(program) = case_program(on, &case) else {
            failures.push(format!("{}: not admitted", case.name));
            return;
        };
        let reference = Program {
            library: off.clone(),
            ..program.clone()
        };
        for slice in [7, 50, u64::MAX] {
            counts.runs += 1;
            let a = finished::<ON>(&program, &case, slice);
            let b = finished::<OFF>(&reference, &case, slice);
            if a != b {
                failures.push(format!(
                    "{}, memory {memory_meter:?}, slice {slice}: {a:?}, {b:?}",
                    case.name
                ));
            }
        }
    }
}

/// What an allocating program's run under plan `L` with slice `slice`
/// shows: its memory counter at each slice return, which no exact count
/// has refined, and its end, its charge and its counter at its end.
fn finished<const L: u64>(
    program: &Program,
    case: &Case,
    slice: u64,
) -> (Vec<u64>, ExpectedEnd, Option<u64>, u64) {
    let start = lash_kernel_vm::CellStart {
        target: lash_kernel_vm::Target::Main,
        args: Vec::new(),
        session: None,
        owner: crate::pages::OWNER,
        modes: Default::default(),
        host: Vec::new(),
    };
    let bounds: lash_kernel_vm::Bounds = case.environment.bounds.into();
    let mut machine = Plan::<L>::start(
        program.clone(),
        bounds,
        start,
        Box::new(crate::pages::Source::default()),
    )
    .unwrap_or_else(|error| panic!("{}: {error}", case.name));
    let mut world = crate::helper_plans::World {
        bits: 0,
        stride: 1,
        nanoseconds: 0,
        prints: Vec::new(),
    };
    let mut returns = Vec::new();
    loop {
        match machine.run(&mut world, slice) {
            Ok(lash_kernel_vm::Step::Slice) => returns.push(machine.meters().memory),
            Ok(lash_kernel_vm::Step::Ended(end)) => {
                let meters = machine.meters();
                return (
                    returns,
                    ExpectedEnd::from_end(end),
                    meters.charged,
                    meters.memory,
                );
            }
            Ok(_) => panic!("{}: a run without effects parked", case.name),
            Err(error) => panic!("{}: {error}", case.name),
        }
    }
}

/// Allocating regions hold what the reference holds (§3.5, §5 R5): each
/// allocating program, run to its end unsliced and under slices, has the
/// memory counter where the reference has it at every slice return and at
/// its end, with memory accounted and off. PLAN-INDEP leaves memory out,
/// so this is the law that sees a region skip a hold.
#[test]
fn allocating_regions_hold_what_the_reference_holds() {
    let registry = Arc::new(super::corpus_registry());
    let on = prepare_with_layout(Arc::clone(&registry), Layout(ON));
    let off = prepare_with_layout(Arc::clone(&registry), Layout(OFF));
    for (name, text) in ALLOCATING {
        for memory_meter in [
            lash_kernel_vm::MemoryMeter::On,
            lash_kernel_vm::MemoryMeter::Off,
        ] {
            let mut case = program_case(name, text, 16);
            case.environment.bounds.memory_meter = memory_meter;
            let program = case_program(&on, &case).expect("an allocating program is admitted");
            let reference = Program {
                library: off.clone(),
                ..program.clone()
            };
            for slice in [7, 50, u64::MAX] {
                assert_eq!(
                    finished::<ON>(&program, &case, slice),
                    finished::<OFF>(&reference, &case, slice),
                    "{name}, memory {memory_meter:?}, slice {slice}"
                );
            }
        }
    }
    // FIG-6758: the dialects' heap paths hold what their bodies hold.
    let dialect = crate::helper_plans::dialect_region_programs();
    for (lowered, program) in &dialect {
        let registry = program.library.registry();
        let on = prepare_with_layout(Arc::clone(registry), Layout(ON));
        let off = prepare_with_layout(Arc::clone(registry), Layout(OFF));
        for memory_meter in [
            lash_kernel_vm::MemoryMeter::On,
            lash_kernel_vm::MemoryMeter::Off,
        ] {
            let mut case = program_case(&lowered.id, "main { return null }", 16);
            case.document = print_document(&program.document);
            case.environment.bounds.memory_meter = memory_meter;
            let program = Program {
                library: on.clone(),
                ..program.clone()
            };
            let reference = Program {
                library: off.clone(),
                ..program.clone()
            };
            for slice in [7, 50, u64::MAX] {
                assert_eq!(
                    finished::<ON>(&program, &case, slice),
                    finished::<OFF>(&reference, &case, slice),
                    "{}, memory {memory_meter:?}, slice {slice}",
                    lowered.id
                );
            }
        }
    }
    println!(
        "allocating regions hold what the reference holds: programs={} paired runs={}",
        ALLOCATING.len() + dialect.len(),
        (ALLOCATING.len() + dialect.len()) * 6
    );
}

/// FIG-6795: linear text accumulators make the run the reference makes
/// (PLAN-INDEP, as [`regions_on_and_off_agree`] states it) and hold what it
/// holds (as [`allocating_regions_hold_what_the_reference_holds`] does),
/// in the kernel's own programs and in both dialects'. A concatenation
/// that took the storage of a text an older version still observes, or
/// gave it back wrong, makes a run read a moved text or disagree. Its own
/// law, so that the shards keep their work.
#[test]
fn linear_text_accumulators_agree_with_the_reference() {
    let began = std::time::Instant::now();
    let registry = Arc::new(super::corpus_registry());
    let kernel = (
        prepare_with_layout(Arc::clone(&registry), Layout(ON)),
        prepare_with_layout(Arc::clone(&registry), Layout(OFF)),
    );
    let mut cases: Vec<(Case, PreparedLibrary, PreparedLibrary)> = ACCUMULATORS
        .iter()
        .map(|(name, text)| {
            (
                program_case(name, text, 16),
                kernel.0.clone(),
                kernel.1.clone(),
            )
        })
        .collect();
    for (lowered, program) in crate::helper_plans::text_accumulator_programs() {
        let registry = program.library.registry();
        let mut case = program_case(&lowered.id, "main { return null }", 16);
        case.document = print_document(&program.document);
        cases.push((
            case,
            prepare_with_layout(Arc::clone(registry), Layout(ON)),
            prepare_with_layout(Arc::clone(registry), Layout(OFF)),
        ));
    }
    agree_with_the_reference(&cases, "text-accumulators", began);
}

/// Each of `cases`, its loop regions formed, makes the run the reference
/// makes (PLAN-INDEP, as [`regions_on_and_off_agree`] states it), with
/// memory off and tight, and holds what it holds (as
/// [`allocating_regions_hold_what_the_reference_holds`] does) at every
/// slice.
fn agree_with_the_reference(
    cases: &[(Case, PreparedLibrary, PreparedLibrary)],
    family: &str,
    began: std::time::Instant,
) {
    let mut counts = Counts::default();
    let mut failures = Vec::new();
    for (case, on, off) in cases {
        let plans = formed_alike(on, off, case);
        assert!(plans.0 > 0, "{}: no loop region forms", case.name);
        let work = check(on, off, case, &mut counts, &mut failures);
        ran(&case.name, plans, work, &mut failures);
        let mut unaccounted = case.clone();
        unaccounted.name = format!("{} (memory off)", case.name);
        unaccounted.environment.bounds.memory_meter = lash_kernel_vm::MemoryMeter::Off;
        check(on, off, &unaccounted, &mut counts, &mut failures);
        let mut tight = case.clone();
        tight.name = format!("{} (memory {TIGHT})", case.name);
        tight.environment.bounds.memory = TIGHT;
        check(on, off, &tight, &mut counts, &mut failures);
        let program = case_program(on, case).expect("the program is admitted");
        let reference = Program {
            library: off.clone(),
            ..program.clone()
        };
        for memory_meter in [
            lash_kernel_vm::MemoryMeter::On,
            lash_kernel_vm::MemoryMeter::Off,
        ] {
            let mut metered = case.clone();
            metered.environment.bounds.memory_meter = memory_meter;
            for slice in [7, 50, u64::MAX] {
                assert_eq!(
                    finished::<ON>(&program, &metered, slice),
                    finished::<OFF>(&reference, &metered, slice),
                    "{}, memory {memory_meter:?}, slice {slice}",
                    case.name
                );
            }
        }
    }
    counts.report(0, family, began);
    assert!(
        failures.is_empty(),
        "{} of {} paired runs disagree:\n{}",
        failures.len(),
        counts.runs,
        failures.join("\n")
    );
}

/// FIG-6846: subscripts that look their key up once per level and
/// f-strings built by concatenation make the run the reference makes and
/// hold what it holds, as [`linear_text_accumulators_agree_with_the_reference`]
/// states it. A lookup that answered a stale or missing entry, or a part
/// concatenated out of order, makes a run disagree.
#[test]
fn single_lookup_subscripts_and_fstrings_agree_with_the_reference() {
    let began = std::time::Instant::now();
    let cases: Vec<(Case, PreparedLibrary, PreparedLibrary)> =
        crate::helper_plans::subscript_and_fstring_programs()
            .into_iter()
            .map(|(lowered, program)| {
                let registry = program.library.registry();
                let mut case = program_case(&lowered.id, "main { return null }", 16);
                case.document = print_document(&program.document);
                (
                    case,
                    prepare_with_layout(Arc::clone(registry), Layout(ON)),
                    prepare_with_layout(Arc::clone(registry), Layout(OFF)),
                )
            })
            .collect();
    agree_with_the_reference(&cases, "subscripts-and-fstrings", began);
}

/// A loop plan retires in the machine whose entries kept missing, never in
/// the code every run of its document shares (FIG-6783). These programs'
/// sweeps hold entries that miss: the cold sum's binding grows past its
/// holding, and the list's elements stop being scalars. Their unsliced
/// runs after the sweep still end iterations in regions, as their natural
/// runs did.
#[test]
fn a_loop_plan_retires_only_in_its_machine() {
    let registry = Arc::new(super::corpus_registry());
    let on = prepare_with_layout(Arc::clone(&registry), Layout(ON));
    let off = prepare_with_layout(registry, Layout(OFF));
    let (mut counts, mut failures) = (Counts::default(), Vec::new());
    let witnesses = [
        PROGRAMS
            .iter()
            .find(|(name, ..)| name.contains("not all scalars"))
            .copied(),
        SESSIONS
            .iter()
            .find(|(name, ..)| name.contains("cold sum"))
            .map(|(name, text, _)| (*name, *text, 16)),
    ];
    for (name, text, call_depth) in witnesses.into_iter().map(|found| found.expect("a witness")) {
        let case = program_case(name, text, call_depth);
        let work = check(&on, &off, &case, &mut counts, &mut failures);
        ran(name, formed(&on, &case), work, &mut failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A `for` region that steps its cursor onto a chunk a resume left cold
/// stops there, and the reference decodes the chunk before it steps on
/// (FIG-6783). K-VAL-034's top-level loop walks 129 elements, past its
/// list's first chunk, and its run resumed from the reference's images
/// at slices 50 and 333 crosses into that chunk inside a region.
#[test]
fn a_for_region_leaves_its_cursor_decoded() {
    let registry = Arc::new(super::corpus_registry());
    let on = prepare_with_layout(Arc::clone(&registry), Layout(ON));
    let off = prepare_with_layout(registry, Layout(OFF));
    let shards = super::corpus_shards();
    let case = shards
        .iter()
        .flat_map(|shard| &shard.cases)
        .find(|case| case.name == "heap nesting is allowed until a copy crosses the boundary")
        .expect("K-VAL-034's nesting case");
    let (mut counts, mut failures) = (Counts::default(), Vec::new());
    let work = check(&on, &off, case, &mut counts, &mut failures);
    assert!(work.iterations > 0, "{}: {work:?}", case.name);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// [`agree`] holds a run that ends at the memory bound in one plan only
/// to the slack the plan dependence allows (FIG-6783): the run that
/// refused charged no more than the other, its charge program is the
/// other's up to its end, and its held bytes there are within the
/// transient hold of the other plan's.
#[test]
fn a_memory_refusal_agrees_only_within_the_transient_slack() {
    let run = |end: ExpectedEnd, charged: u64, tape: Vec<Weight>, held: u64| Probe {
        observations: Observations {
            prints: Vec::new(),
            end,
            trace: Vec::new(),
            charged: Some(charged),
            parks: 0,
        },
        tape,
        projector: Projector {
            held: Some(held),
            ..Projector::default()
        },
        work: RegionWork::default(),
    };
    let refused = |charged: u64, tape: Vec<Weight>, held: u64| {
        let end = ExpectedEnd::Bound {
            name: "memory".into(),
            limit: 1 << 20,
        };
        run(end, charged, tape, held)
    };
    let finished = run(
        ExpectedEnd::Finished(lash_kernel_doc::Datum::Null),
        12,
        vec![Weight::Units(10), Weight::Atomic(2)],
        1 << 19,
    );
    let mut there = |_: bool, _: u64| Some(1 << 20);
    // Within the slack: a prefix of the other's charges, held near it.
    let within = refused(10, vec![Weight::Units(10)], (1 << 20) - 64);
    assert!(agree(&within, &finished, &mut there));
    assert!(agree(&finished, &within, &mut there));
    // A refusal past the other run's end.
    let later = refused(14, vec![Weight::Units(10), Weight::Atomic(4)], 1 << 20);
    assert!(!agree(&later, &finished, &mut there));
    // A charge program the other run does not make.
    let apart = refused(10, vec![Weight::Atomic(10)], 1 << 20);
    assert!(!agree(&apart, &finished, &mut there));
    // Held bytes past the transient hold of the other plan's there.
    let mut short = |_: bool, _: u64| Some(0);
    let held = refused(10, vec![Weight::Units(10)], transient_hold() + 1);
    assert!(!agree(&held, &finished, &mut short));
}

/// A host's charge limiter that grows the limit once by its amount, then
/// refuses.
struct GrowOnce(Option<u64>);

impl lash_kernel_vm::ChargeLimiter for GrowOnce {
    fn limit_reached(&mut self, _: u64, _: u64) -> lash_kernel_vm::LimitDecision {
        self.0
            .take()
            .map_or(lash_kernel_vm::LimitDecision::Refuse, |grow| {
                lash_kernel_vm::LimitDecision::Grow(grow)
            })
    }
}

/// What a run observably did: its end, its charge, its prints and its
/// expanded charge program.
type Ran = (
    ExpectedEnd,
    Option<u64>,
    Vec<lash_kernel_doc::Datum>,
    Vec<Weight>,
);

/// How `program`'s run under plan `L` ends at charge limit `limit`, with a
/// limiter that grows it once by `grow` when given, and the work regions
/// committed in it.
fn limited<const L: u64>(
    program: &Program,
    name: &str,
    limit: u64,
    grow: Option<u64>,
) -> (Ran, RegionWork) {
    let (ended, tape, work) = taped_work(|| {
        let start = lash_kernel_vm::CellStart {
            target: lash_kernel_vm::Target::Main,
            args: Vec::new(),
            session: None,
            owner: crate::pages::OWNER,
            modes: Default::default(),
            host: Vec::new(),
        };
        let mut environment = Environment::default();
        environment.bounds.call_depth = 16;
        let bounds = lash_kernel_vm::Bounds {
            charge_limit: limit,
            ..environment.bounds.into()
        };
        let mut machine = Plan::<L>::start(
            program.clone(),
            bounds,
            start,
            Box::new(crate::pages::Source::default()),
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        machine.set_options(lash_kernel_vm::MachineOptions {
            charge_limiter: grow.map(|grow| {
                Box::new(GrowOnce(Some(grow))) as Box<dyn lash_kernel_vm::ChargeLimiter>
            }),
            ..Default::default()
        });
        let mut world = crate::helper_plans::World {
            bits: 0,
            stride: 1,
            nanoseconds: 0,
            prints: Vec::new(),
        };
        loop {
            match machine.run(&mut world, u64::MAX) {
                Ok(lash_kernel_vm::Step::Ended(end)) => {
                    return (
                        ExpectedEnd::from_end(end),
                        machine.meters().charged,
                        world.prints,
                    );
                }
                Ok(lash_kernel_vm::Step::Slice) => {}
                Ok(_) => panic!("{name}: a run without effects parked"),
                Err(error) => panic!("{name}: {error}"),
            }
        }
    });
    let (end, charged, prints) = ended;
    let ran = (end, charged, prints, Weight::expand(tape.iter().copied()));
    (ran, work)
}

/// FIG-6781: a region statement's heap writes commit with the statement
/// (`docs/design/kernel-regions.md` §1: a failed guard changes nothing a
/// run can see). A statement that misses after a template wrote a guest
/// object, here a read past a list its own append grew, leaves the write
/// to the reference, which makes it once: regions on finish as off do.
/// And the FIG-6768 limiter comparison over the dialects' heap-path
/// programs: a run whose limiter grows its charge limit where the run
/// reaches it, at every limit around its charge program's events, runs
/// as one started with the grown limit, with regions on and off alike,
/// though the crossing falls inside a statement whose template wrote.
#[test]
fn region_heap_writes_commit_with_their_statement() {
    let programs = crate::helper_plans::heap_write_programs();
    let (mut runs, mut failures) = (0, Vec::new());
    for (lowered, program) in &programs {
        let name = &lowered.id;
        let registry = program.library.registry();
        let on = Program {
            library: prepare_with_layout(Arc::clone(registry), Layout(ON)),
            ..program.clone()
        };
        let off = Program {
            library: prepare_with_layout(Arc::clone(registry), Layout(OFF)),
            ..program.clone()
        };
        let roomy = Environment::default().bounds.charge;
        let mut case = program_case(name, "main { return null }", 16);
        case.document = print_document(&program.document);
        assert!(
            formed(&on.library, &case).0 > 0,
            "{name}: no loop region forms"
        );
        let (natural, work) = limited::<ON>(&on, name, roomy, None);
        let (reference, _) = limited::<OFF>(&off, name, roomy, None);
        // The witnesses' writes run in regions; only the append its own
        // statement then reads past is left to the reference.
        if name.contains("heap write: ") && !name.ends_with("reads past") && work.iterations == 0 {
            failures.push(format!("{name}: no region iteration ran: {work:?}"));
        }
        if natural != reference {
            failures.push(format!(
                "{name}: regions on {:?} {:?} charged {:?}, off {:?} {:?} charged {:?}",
                natural.0, natural.2, natural.1, reference.0, reference.2, reference.1
            ));
            continue;
        }
        let total = natural.1.expect("charge is metered");
        let grow = total;
        for limit in bounds(&natural.3, total) {
            runs += 3;
            let (grown, _) = limited::<ON>(&on, name, limit, Some(grow));
            let (started, _) = limited::<ON>(&on, name, limit + grow, None);
            let (off, _) = limited::<OFF>(&off, name, limit, Some(grow));
            if grown != started || grown != off {
                failures.push(format!(
                    "{name}: limit {limit} grown by {grow}: regions on {:?} {:?} charged {:?}, \
                     started with it {:?} {:?} charged {:?}, regions off {:?} {:?} charged {:?}",
                    grown.0, grown.2, grown.1, started.0, started.2, started.1, off.0, off.2, off.1
                ));
                break;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} programs disagree:\n{}",
        failures.len(),
        failures.join("\n")
    );
    println!(
        "region heap writes commit with their statement: programs={} runs={runs}",
        programs.len()
    );
}

/// FIG-6845: a TypeScript `for...of` over an array inside a called
/// function runs its iterations in its loop region, as Python's `for...in`
/// over the same list does (`docs/design/kernel-regions.md` §1, loop
/// region): the `ts.for_of.item` hook answers the array's items on its
/// generated path, so the two dialects' loops commit the same iterations
/// in their regions and finish as the reference does.
#[test]
fn typescript_for_of_over_an_array_runs_in_its_region_as_python_for_in_does() {
    let programs = crate::helper_plans::island_programs();
    let mut iterations = Vec::new();
    for id in [
        "typescript islands: texts echoed by root call actions in a loop",
        "python islands: texts echoed in a loop",
    ] {
        let (_, program) = programs
            .iter()
            .find(|(lowered, _)| lowered.id == id)
            .unwrap_or_else(|| panic!("{id} is an island program"));
        let registry = program.library.registry();
        let on = Program {
            library: prepare_with_layout(Arc::clone(registry), Layout(ON)),
            ..program.clone()
        };
        let off = Program {
            library: prepare_with_layout(Arc::clone(registry), Layout(OFF)),
            ..program.clone()
        };
        let roomy = Environment::default().bounds.charge;
        let (natural, work) = limited::<ON>(&on, id, roomy, None);
        let (reference, _) = limited::<OFF>(&off, id, roomy, None);
        assert_eq!(natural, reference, "{id}: regions on and off disagree");
        iterations.push((id, work.iterations));
    }
    let [(_, typescript), (_, python)] = iterations[..] else {
        unreachable!("two programs ran")
    };
    assert!(
        python > 0,
        "the Python loop runs in its region: {iterations:?}"
    );
    assert_eq!(
        typescript, python,
        "the TypeScript loop commits the iterations Python's does: {iterations:?}"
    );
}

/// FIG-6903: TypeScript callbacks that read a record's field run as
/// islands (`docs/design/kernel-regions.md` §1, call island): the
/// comparator a sort applies and the callbacks map and filter apply read
/// the field in place behind the lowering's record guard, its
/// `same(x.__ts_brand, ())` answered without building the tuple, and its
/// fallback statements cold. Each forms an island, its natural run enters
/// them, and under PLAN-INDEP's sweep it runs as the reference runs it:
/// end, charge, tape and images, where the callbacks stay in their islands
/// and where they leave them (no record, a branded one, a missing field, a
/// field no island frame holds, a raise).
#[test]
fn record_field_callbacks_run_as_islands_as_the_reference_runs_them() {
    let mut counts = Counts::default();
    let mut failures = Vec::new();
    for (lowered, program, callbacks) in crate::helper_plans::record_field_island_programs() {
        let registry = program.library.registry();
        let on = prepare_with_layout(Arc::clone(registry), Layout(ON));
        let off = prepare_with_layout(Arc::clone(registry), Layout(OFF));
        let mut case = program_case(&lowered.id, "main { return null }", 16);
        case.document = print_document(&program.document);
        let plans = formed_alike(&on, &off, &case);
        assert!(
            plans.1 >= callbacks,
            "{}: {} call islands form for its {callbacks} record callbacks",
            case.name,
            plans.1
        );
        for call_depth in [3, 16] {
            case.environment.bounds.call_depth = call_depth;
            let work = check(&on, &off, &case, &mut counts, &mut failures);
            if call_depth == 16 {
                ran(&case.name, plans, work, &mut failures);
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} paired runs disagree:\n{}",
        failures.len(),
        counts.runs,
        failures.join("\n")
    );
}

macro_rules! program_shards {
    ($($name:ident: $shard:expr),+ $(,)?) => {
        $(
            #[test]
            fn $name() {
                regions_on_and_off_agree($shard);
            }
        )+
    };
}

program_shards! {
    regions_on_and_off_agree_programs_0: 0,
    regions_on_and_off_agree_programs_1: 1,
    regions_on_and_off_agree_programs_2: 2,
    regions_on_and_off_agree_programs_3: 3,
    regions_on_and_off_agree_programs_4: 4,
    regions_on_and_off_agree_programs_5: 5,
    regions_on_and_off_agree_programs_6: 6,
    regions_on_and_off_agree_programs_7: 7,
    regions_on_and_off_agree_programs_8: 8,
    regions_on_and_off_agree_programs_9: 9,
    regions_on_and_off_agree_programs_10: 10,
    regions_on_and_off_agree_programs_11: 11,
    regions_on_and_off_agree_programs_12: 12,
    regions_on_and_off_agree_programs_13: 13,
    regions_on_and_off_agree_programs_14: 14,
    regions_on_and_off_agree_programs_15: 15,
}
