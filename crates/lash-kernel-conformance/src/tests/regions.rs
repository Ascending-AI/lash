//! PLAN-INDEP of regions (`docs/design/kernel-regions.md` §3.3): a run
//! with regions on and the same run with them off, through the layout bit
//! and with everything else equal, make the same run: the same end and
//! charge, the same expanded charge program on the tape, the same prints,
//! parks, requests and effect ids, and at every slice return and park the
//! same sealed semantic checkpoint, with a checkpoint sealed under either
//! plan resuming under the other.

use std::sync::Arc;

use lash_kernel_doc::{Document, Name, parse_document, print_document};
use lash_kernel_exec::charge::Weight;
use lash_kernel_vm::kernel_internals::{Layout, call_islands, loop_regions, prepare_with_layout};
use lash_kernel_vm::{Machine, PreparedLibrary, Program};

use crate::machine::{Run, case_program, run_program_in};
use crate::projection::{Image, Plan, Projector, corpus_agrees, taped};
use crate::{Case, Environment, Expected, ExpectedEnd, HarnessError, Observations};

/// Regions on: the natural layout.
const ON: u64 = 0;
/// Regions off, and everything else equal.
const OFF: u64 = Layout(ON).without_regions().0;
/// The slices every case runs under, the last unbounded.
const SLICES: [u64; 7] = [1, 2, 3, 7, 50, 333, u64::MAX];
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

/// Independent libtest laws, assigned to the target's existing Buck shards.
const SHARDS: usize = 8;

/// One run of a case under a plan: what it observed, its charge program
/// and its projections.
struct Probe<'a> {
    observations: Observations,
    tape: Vec<Weight>,
    projector: Projector<'a>,
}

/// Runs `case` under plan `L`, bounded at `charge` when given, sealing at
/// every slice return and park and, with `imports`, resuming the other
/// plan's image there.
fn probe<'a, const L: u64>(
    program: &Program,
    case: &Case,
    charge: Option<u64>,
    imports: Option<&'a [Image]>,
) -> Result<Probe<'a>, HarnessError> {
    let mut projector = Projector {
        imports,
        layout: Layout(L),
        library: Some(program.library.clone()),
        ..Projector::default()
    };
    let (observations, tape) = taped(|| {
        let mut start = |program, bounds, start, source: &crate::pages::Source| {
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
    })
}

/// Whether two runs agree: the run, its checkpoints and its charge
/// program; before a memory refusal, whose point may depend on the plan
/// within the transient slack (§3.5), their common prefix.
fn agree(a: &Probe<'_>, b: &Probe<'_>) -> bool {
    let memory =
        |end: &ExpectedEnd| matches!(end, ExpectedEnd::Bound { name, .. } if name == "memory");
    corpus_agrees(&a.observations, &b.observations, &a.projector, &b.projector)
        && (memory(&a.observations.end)
            || memory(&b.observations.end)
            || Weight::expand(a.tape.iter().copied()) == Weight::expand(b.tape.iter().copied()))
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
}

impl Counts {
    fn report(&self, shard: usize, family: &str, began: std::time::Instant) {
        println!(
            "regions shard={shard} through={family}: cases={} pairs={} bounds={} slices={} cross_resumes={} slice_bounds={} smith_schedules={} smith_bounds={} seconds={:.3}",
            self.cases,
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

/// Adds a disagreement of runs `a` and `b` of `case` to `failures`.
fn compare(
    case: &Case,
    what: &str,
    a: Result<&Probe<'_>, &HarnessError>,
    b: Result<&Probe<'_>, &HarnessError>,
    counts: &mut Counts,
    failures: &mut Vec<String>,
) {
    counts.runs += 1;
    match (a, b) {
        (Ok(a), Ok(b)) if agree(a, b) => {}
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

/// Checks `case` under both plans across the sweep, adding each
/// disagreement to `failures`.
fn check(
    on: &PreparedLibrary,
    off: &PreparedLibrary,
    case: &Case,
    counts: &mut Counts,
    failures: &mut Vec<String>,
) {
    let mut case = case.clone();
    case.environment.resume = false;
    case.environment.slice = u64::MAX;
    let Some(on) = case_program(on, &case) else {
        return;
    };
    let off = Program {
        library: off.clone(),
        ..on.clone()
    };
    let Ok(natural) = probe::<ON>(&on, &case, None, None) else {
        // A case the harness cannot run is the corpus tests' to report.
        return;
    };
    if natural.observations.end == ExpectedEnd::Refused {
        return;
    }
    counts.cases += 1;
    let program = Weight::expand(natural.tape.iter().copied());
    let whole = natural.observations.charged.unwrap_or(0);
    for bound in bounds(&program, case.environment.bounds.charge) {
        counts.bounds += 1;
        let a = probe::<ON>(&on, &case, Some(bound), None);
        let b = probe::<OFF>(&off, &case, Some(bound), None);
        compare(
            &case,
            &format!("bound {bound}"),
            a.as_ref(),
            b.as_ref(),
            counts,
            failures,
        );
    }
    for slice in SLICES {
        counts.slices += 1;
        let mut sliced = case.clone();
        sliced.environment.slice = slice;
        sliced.environment.max_steps = sliced.environment.max_steps.max(1_000_000);
        let a = probe::<ON>(&on, &sliced, None, None);
        let b = probe::<OFF>(&off, &sliced, None, None);
        compare(
            &sliced,
            &format!("slice {slice}"),
            a.as_ref(),
            b.as_ref(),
            counts,
            failures,
        );
        // Cross-resume: each plan resumes the other's images, which the
        // comparison above found equal, so the two resumed runs are one
        // resume under either plan. A resumed run is compared with a
        // resumed run: where a resume prices its memory against a
        // continuous run is the slice law's, not the plans'.
        if let (Ok(a), Ok(b)) = (&a, &b) {
            counts.resumes += 1;
            let off_resumed = probe::<OFF>(&off, &sliced, None, Some(&a.projector.images));
            let on_resumed = probe::<ON>(&on, &sliced, None, Some(&b.projector.images));
            let what = format!("slice {slice}, each resuming the other's images");
            compare(
                &sliced,
                &what,
                on_resumed.as_ref(),
                off_resumed.as_ref(),
                counts,
                failures,
            );
        }
        for bound in [whole / 3, whole / 2, whole.saturating_sub(5)] {
            counts.slice_bounds += 1;
            let a = probe::<ON>(&on, &sliced, Some(bound), None);
            let b = probe::<OFF>(&off, &sliced, Some(bound), None);
            let what = format!("slice {slice}, bound {bound}");
            compare(&sliced, &what, a.as_ref(), b.as_ref(), counts, failures);
        }
    }
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

/// PLAN-INDEP of regions (`docs/design/kernel-regions.md` §3.3): every
/// corpus case, the 64-seed smith programs, the region programs and the
/// call-island programs, each
/// as written and wrapped in a function, run alike with regions on and
/// off, under every bound around their charge programs' events and every
/// slice, with each plan resuming the other's checkpoints.
fn regions_on_and_off_agree(shard: usize) {
    let began = std::time::Instant::now();
    let registry = Arc::new(super::corpus_registry());
    let on = prepare_with_layout(Arc::clone(&registry), Layout(ON));
    let off = prepare_with_layout(Arc::clone(&registry), Layout(OFF));
    let mut counts = Counts::default();
    let mut failures = Vec::new();
    let mut regions = 0;
    let programs = PROGRAMS
        .iter()
        .enumerate()
        .filter_map(|(index, program)| (index % SHARDS == shard).then_some(program))
        .chain(EXIT_PROGRAMS);
    let mut region_programs = 0;
    for (name, text, call_depth) in programs {
        region_programs += 1;
        let case = program_case(name, text, *call_depth);
        let (formed_on, _) = formed(&on, &case);
        assert!(formed_on > 0, "{name}: no loop region forms");
        assert_eq!(
            formed(&off, &case),
            (0, 0),
            "{name}: regions form with regions off"
        );
        regions += formed_on;
        check(&on, &off, &case, &mut counts, &mut failures);
    }
    for (index, (name, text)) in ALLOCATING.iter().enumerate() {
        if index % SHARDS != shard {
            continue;
        }
        region_programs += 1;
        let case = program_case(name, text, 16);
        let (formed_on, _) = formed(&on, &case);
        assert!(formed_on > 0, "{name}: no loop region forms");
        assert_eq!(
            formed(&off, &case),
            (0, 0),
            "{name}: regions form with regions off"
        );
        regions += formed_on;
        check(&on, &off, &case, &mut counts, &mut failures);
        let mut unaccounted = case.clone();
        unaccounted.name = format!("{name} (memory off)");
        unaccounted.environment.bounds.memory_limit = lash_kernel_vm::MemoryLimit::Off;
        check(&on, &off, &unaccounted, &mut counts, &mut failures);
        let mut tight = case;
        tight.name = format!("{name} (memory {TIGHT})");
        tight.environment.bounds.memory = TIGHT;
        check(&on, &off, &tight, &mut counts, &mut failures);
    }
    for (index, (name, text, parks)) in SESSIONS.iter().enumerate() {
        if index >= SESSION_WITNESSES && index % SHARDS != shard {
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
        let (formed_on, _) = formed(&on, &case);
        assert!(formed_on > 0, "{name}: no loop region forms");
        assert_eq!(
            formed(&off, &case),
            (0, 0),
            "{name}: regions form with regions off"
        );
        regions += formed_on;
        check(&on, &off, &case, &mut counts, &mut failures);
        if !*parks {
            sessions_hold(&on, &off, &case, &mut counts, &mut failures);
        }
    }
    for (index, (lowered, program)) in crate::helper_plans::top_level_region_programs()
        .into_iter()
        .enumerate()
    {
        if index % SHARDS != shard {
            continue;
        }
        region_programs += 1;
        let registry = program.library.registry();
        let on = prepare_with_layout(Arc::clone(registry), Layout(ON));
        let off = prepare_with_layout(Arc::clone(registry), Layout(OFF));
        let mut case = program_case(&lowered.id, "main { return null }", 16);
        case.document = print_document(&program.document);
        let (formed_on, _) = formed(&on, &case);
        assert!(formed_on > 0, "{}: no loop region forms", case.name);
        assert_eq!(formed(&off, &case), (0, 0), "{}", case.name);
        regions += formed_on;
        check(&on, &off, &case, &mut counts, &mut failures);
        sessions_hold(&on, &off, &case, &mut counts, &mut failures);
    }
    counts.report(shard, "region-programs", began);
    let (mut island_programs, mut islands) = (0, 0);
    for (name, text, call_depth) in ISLANDS
        .iter()
        .enumerate()
        .filter_map(|(index, program)| (index % SHARDS == shard).then_some(program))
    {
        island_programs += 1;
        let case = program_case(name, text, *call_depth);
        let (_, formed_on) = formed(&on, &case);
        assert!(formed_on > 0, "{name}: no call island forms");
        assert_eq!(
            formed(&off, &case),
            (0, 0),
            "{name}: islands form with regions off"
        );
        islands += formed_on;
        check(&on, &off, &case, &mut counts, &mut failures);
    }
    counts.report(shard, "island-programs", began);
    // FIG-6754: actual Python invokes preserve their atom/action charge,
    // occurrences and continuations, including declines and zero division.
    // Assign each extra program once, like the existing program families.
    for (index, (lowered, program)) in crate::helper_plans::python_region_programs()
        .into_iter()
        .enumerate()
    {
        if index % SHARDS != shard {
            continue;
        }
        let registry = program.library.registry();
        let on = prepare_with_layout(Arc::clone(registry), Layout(ON));
        let off = prepare_with_layout(Arc::clone(registry), Layout(OFF));
        let mut case = program_case(&lowered.id, "main { return null }", 16);
        case.document = print_document(&program.document);
        assert!(formed(&on, &case).0 > 0, "{}", case.name);
        assert_eq!(formed(&off, &case), (0, 0));
        for call_depth in [1, 2, 3, 4, 16] {
            case.environment.bounds.call_depth = call_depth;
            check(&on, &off, &case, &mut counts, &mut failures);
        }
    }
    counts.report(shard, "python-invokes", began);
    for (index, case) in super::corpus_shards()
        .iter()
        .flat_map(|shard| &shard.cases)
        .enumerate()
    {
        if index % SHARDS == shard {
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
/// and at its end, durable and ephemeral, with memory accounted and off.
/// PLAN-INDEP leaves memory out, so this is what sees a region skip a
/// session write's admission.
fn sessions_hold(
    on: &PreparedLibrary,
    off: &PreparedLibrary,
    case: &Case,
    counts: &mut Counts,
    failures: &mut Vec<String>,
) {
    for durability in [
        lash_kernel_vm::Durability::Durable,
        lash_kernel_vm::Durability::Ephemeral,
    ] {
        for memory_limit in [
            lash_kernel_vm::MemoryLimit::Recount,
            lash_kernel_vm::MemoryLimit::Off,
        ] {
            let mut case = case.clone();
            case.environment.bounds.memory_limit = memory_limit;
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
                let a = finished::<ON>(&program, &case, slice, durability);
                let b = finished::<OFF>(&reference, &case, slice, durability);
                if a != b {
                    failures.push(format!(
                        "{}, {durability:?}, memory {memory_limit:?}, slice {slice}: {a:?}, {b:?}",
                        case.name
                    ));
                }
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
    durability: lash_kernel_vm::Durability,
) -> (Vec<u64>, ExpectedEnd, Option<u64>, u64) {
    let start = lash_kernel_vm::CellStart {
        target: lash_kernel_vm::Target::Main,
        args: Vec::new(),
        session: None,
        owner: crate::pages::OWNER,
        modes: Default::default(),
        host: Vec::new(),
    };
    let mut bounds: lash_kernel_vm::Bounds = case.environment.bounds.into();
    bounds.durability = durability;
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
        for memory_limit in [
            lash_kernel_vm::MemoryLimit::Recount,
            lash_kernel_vm::MemoryLimit::Off,
        ] {
            let mut case = program_case(name, text, 16);
            case.environment.bounds.memory_limit = memory_limit;
            let program = case_program(&on, &case).expect("an allocating program is admitted");
            let reference = Program {
                library: off.clone(),
                ..program.clone()
            };
            for slice in [7, 50, u64::MAX] {
                let durable = lash_kernel_vm::Durability::Durable;
                assert_eq!(
                    finished::<ON>(&program, &case, slice, durable),
                    finished::<OFF>(&reference, &case, slice, durable),
                    "{name}, memory {memory_limit:?}, slice {slice}"
                );
            }
        }
    }
    println!(
        "allocating regions hold what the reference holds: programs={} paired runs={}",
        ALLOCATING.len(),
        ALLOCATING.len() * 6
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
}
