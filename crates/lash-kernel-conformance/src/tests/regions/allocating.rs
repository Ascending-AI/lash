// Allocating-region statement and append boundary matrices.

/// FIG-6964 (`docs/design/kernel-regions.md` §2.5): allocating loop
/// regions that append to lists, in a body and an `if` arm, over a list's
/// items, and to a variable that stops holding a list, whose store then
/// runs alone and whose `list.len` raises.
const APPENDS: &[(&str, &str)] = &[
    (
        "appends in a body and an arm",
        "definition \"function build(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let i = 0 let xs = [] let ys = [] while num.lt(i, n) { set xs[list.len(xs)] = num.mul(i, 2) if num.lt(num.rem_trunc(i, 3), 1) { set ys[list.len(ys)] = [i, list.len(xs)] } set i = num.add(i, 1) } return (xs, ys) }\"\nmain { let r = call build(12) finish r }",
    ),
    (
        "appends over a list's items",
        "definition \"function double(src: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let out = [] for x in src { set out[list.len(out)] = num.mul(x, 2) } return out }\"\nmain { let r = call double([1, 2, 3, 4, 5, 6, 7, 8]) finish r }",
    ),
    (
        "an append whose variable stops holding a list",
        "definition \"function swap(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let i = 0 let t = [] while num.lt(i, n) { set t[list.len(t)] = i if num.lt(4, i) { set t = map{} } set i = num.add(i, 1) } return t }\"\nmain { try { let r = call swap(9) finish r } catch e { finish e.kind } }",
    ),
];

/// FIG-6964: an allocating region's statements admit, run and commit one
/// by one, and a list append reads its list's length once. Each program's
/// natural run commits appends, and across the sweep, with charge Exact
/// and Off, under every slice, with memory off and under a tight memory
/// bound, a run with regions on stops where the reference stops: at its
/// statement, with its charge, its charge program and its images.
#[test]
fn list_appends_stop_where_the_reference_stops() {
    stop_where_the_reference_stops(
        "list appends",
        APPENDS,
        crate::helper_plans::append_programs(),
        |work| work.appends > 0,
        Sweep::default(),
    );
}

/// Programs whose allocating loops run straight-line statements whose
/// charges are bounded (FIG-7001): runs ending in an `if` whose arms hold
/// runs of their own and in a `continue`, a run holding a big integer
/// product whose formula measures past its table's most, and runs between
/// statements whose formulas no most bounds.
const RUNS: &[(&str, &str)] = &[
    (
        "runs around a list literal and an index write",
        "definition \"function rows(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let i = 0 let s = 0 let kept = [0, 0] while num.lt(i, n) { let a = num.mul(i, 2) let b = num.add(a, 1) set kept[0] = [a, b] set s = num.add(s, b) set i = num.add(i, 1) } return (s, kept) }\"\nmain { let r = call rows(12) finish r }",
    ),
    (
        "a run whose big integer product measures past its most",
        "definition \"function grow(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let i = 0 let x = 3 let r = [0] while num.lt(i, n) { let a = num.add(i, 1) set x = num.mul(x, 7) set r = [a] set i = num.add(i, 1) } return (x, r) }\"\nmain { let r = call grow(30) finish r }",
    ),
    (
        "runs ending in an if and a continue",
        "definition \"function mix(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let i = 0 let s = 0 let t = [0] while num.lt(i, n) { set i = num.add(i, 1) let a = num.mul(i, 3) if num.lt(num.rem_trunc(i, 3), 1) { let b = num.add(a, 2) set t = [b] set s = num.add(s, b) continue } set t = [a] set s = num.sub(s, a) } return (s, t) }\"\nmain { let r = call mix(13) finish r }",
    ),
    (
        "runs between text concatenations",
        "definition \"function label(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let i = 0 let s = \\\"\\\" let k = [0] while num.lt(i, n) { let a = num.add(i, 2) set s = text.concat(s, num.to_text(a)) set k = [a, i] set i = num.add(i, 1) } return (s, k) }\"\nmain { let r = call label(11) finish r }",
    ),
];

/// FIG-7001: an allocating region admits each straight-line run of its
/// bounded statements once, where the bound admits the run's most, and
/// leaves a statement that charges past its own most, and every statement
/// after it in the run, to admit on its own. Each program's natural run
/// ends region iterations, and across the sweep, with charge Exact and
/// Off, under every slice, with memory off and under a tight memory
/// bound, a run with regions on stops where the reference stops: a bound
/// that falls inside a run stops it at the reference's statement, with
/// the reference's charge.
#[test]
fn statement_runs_stop_where_the_reference_stops() {
    statement_runs_sweep(0);
}

#[test]
fn statement_runs_stop_where_the_reference_stops_sweep_1() {
    statement_runs_sweep(1);
}

#[test]
fn statement_runs_stop_where_the_reference_stops_sweep_2() {
    statement_runs_sweep(2);
}

#[test]
fn statement_runs_stop_where_the_reference_stops_sweep_3() {
    statement_runs_sweep(3);
}

// FIG-7093: the full 5,808-pair matrix exceeds the action bound on a slow
// worker. Deal independent comparisons across four laws with the existing
// exhaustive Sweep; keep each slice with its cross-resumes and bounds.
fn statement_runs_sweep(part: usize) {
    stop_where_the_reference_stops(
        "statement runs",
        RUNS,
        crate::helper_plans::statement_run_programs(),
        |work| work.iterations > 0,
        Sweep { part, parts: 4 },
    );
}

/// Checks the kernel's `programs` and the dialects' `lowered` programs
/// across the sweep, each as it is, with memory off and under a tight
/// memory bound, where each one's natural run with regions on did the
/// work `ran` names.
fn stop_where_the_reference_stops(
    what: &str,
    programs: &[(&str, &str)],
    lowered: Vec<(crate::helper_plans::runner::Case, Program)>,
    ran: fn(&RegionWork) -> bool,
    sweep: Sweep,
) {
    let registry = Arc::new(super::corpus_registry());
    let mut cases = Vec::new();
    for (name, text) in programs {
        let on = prepare_with_layout(Arc::clone(&registry), Layout(ON));
        let off = prepare_with_layout(Arc::clone(&registry), Layout(OFF));
        cases.push((program_case(name, text, 16), on, off));
    }
    for (lowered, program) in lowered {
        let prepared = &program.library;
        let on = in_layout(prepared, Layout(ON));
        let off = in_layout(prepared, Layout(OFF));
        let mut case = program_case(&lowered.id, "main { return null }", 16);
        case.document = print_document(&program.document);
        cases.push((case, on, off));
    }
    let mut counts = Counts {
        sweep,
        ..Counts::default()
    };
    let mut failures = Vec::new();
    for (case, on, off) in &cases {
        let work = check(on, off, case, &mut counts, &mut failures);
        if !ran(&work) {
            failures.push(format!(
                "{}: its natural run does not do the work the law is about: {work:?}",
                case.name
            ));
        }
        let mut unaccounted = case.clone();
        unaccounted.name = format!("{} (memory off)", case.name);
        unaccounted.environment.bounds.memory_meter = lash_kernel_vm::MemoryMeter::Off;
        check(on, off, &unaccounted, &mut counts, &mut failures);
        let mut tight = case.clone();
        tight.name = format!("{} (memory {TIGHT})", case.name);
        tight.environment.bounds.memory = TIGHT;
        check(on, off, &tight, &mut counts, &mut failures);
    }
    assert!(
        failures.is_empty(),
        "{} of {} paired runs disagree:\n{}",
        failures.len(),
        counts.runs,
        failures.join("\n")
    );
    println!(
        "{what}: sweep={}/{} programs={} paired runs={} bounds={} slices={}",
        sweep.part,
        sweep.parts,
        cases.len(),
        counts.runs,
        counts.bounds,
        counts.slices
    );
}
