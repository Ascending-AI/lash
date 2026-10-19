//! Library bodies that `perform` (`K-LIB-004`), as CONT-EXACT runs them:
//! each case runs uninterrupted, at slices MAX, 1 and 0, and sealed and
//! opened cold at every slice and park, and also after every delivery
//! before the delivered task consumes its outcome. Every run leaves the
//! uninterrupted run's end, prints, requests (identities and origins
//! included), reads and charge.
//!
//! - ORIGIN: an effect's origin is the nearest authored statement, across
//!   library calls, spawns and reads, and survives a seal.
//! - K-CHG-007: a body perform pays its atoms, its arguments' copy-out and
//!   crossed function bytes, and its result's decode or consumption, once,
//!   whether or not the run was sealed between delivery and consumption.

use std::collections::BTreeMap;

use lash_kernel_doc::{Datum, FunctionId, FunctionRegistry, Name, Site, Unit, parse_definition};

use super::embedder::{Embedder, Setup, wait_of};
use super::{Drive, Trace, bodies, everywhere, int, list, text};
use crate::{End, Machine, Outcome, Request, Step};

/// Library functions whose bodies perform, each with only a kernel body
/// and the completion formula 1:
/// - `pkg.fetch(x)` performs `echo(x)` as a statement and gives the result;
/// - `pkg.fetch_nested(x)` performs it nested in an expression;
/// - `pkg.wrap(x)` gives what `pkg.fetch(x)` gives, a library call between;
/// - `pkg.fetch_yield(x)` performs it `then yield`, and `pkg.await` is an
///   await policy that takes one turn, as `ts.await` does for a value that
///   is not a promise;
/// - `pkg.each(xs)` performs `echo` once per item, in a loop;
/// - `pkg.guarded(x)` performs `echo(x)` in a `try` whose `finally`
///   performs `echo("cleanup")`;
/// - `pkg.start(x)` spawns `pkg.fetch(x)` and gives the task;
/// - `pkg.discard(x)` performs `echo(x)` and drops its result;
///   `pkg.noop(x)` does nothing.
fn packages(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut out = bodies(registry, ids);
    let mut defined: BTreeMap<&'static str, FunctionId> = BTreeMap::new();
    let mut register = |name: &'static str, uses: &[&'static str], body: &str| {
        let mut header = format!(
            "function {name}(x: Any) -> Any\nkernel 1\ncharge 1\nuse list.len = @{}\n",
            ids["list.len"]
        );
        for used in uses {
            header.push_str(&format!("use {used} = @{}\n", defined[used]));
        }
        // A body lists the operations it performs itself (`K-LIB-004`),
        // under the requirement the laws' documents state for `echo`.
        if body.contains("perform ") {
            header.push_str("effect echo(x?: Any) -> Any\n");
        }
        let definition = parse_definition(&format!("{header}body {{ {body} }}\n")).unwrap();
        let id = registry.register(definition, None).unwrap();
        defined.insert(name, id);
        (name, id)
    };
    out.extend([
        register("pkg.fetch", &[], "let v = perform echo(x) as Any return v"),
        register(
            "pkg.fetch_nested",
            &[],
            "let v = [perform echo(x) as Any] return v[0]",
        ),
        register(
            "pkg.wrap",
            &["pkg.fetch"],
            "let v = invoke pkg.fetch(x) return v",
        ),
        register(
            "pkg.fetch_yield",
            &[],
            "let v = perform echo(x) as Any then yield return v",
        ),
        register("pkg.await", &[], "do yield return x"),
        register(
            "pkg.each",
            &[],
            "let out = [] for item in x { let v = perform echo(item) as Any \
             set out[list.len(out)] = v } return out",
        ),
        register(
            "pkg.guarded",
            &[],
            "try { let v = perform echo(x) as Any return v } \
             finally { do perform echo(\"cleanup\") as Any }",
        ),
        register(
            "pkg.start",
            &["pkg.fetch"],
            "let h = spawn invoke pkg.fetch(x) return h",
        ),
        register("pkg.discard", &[], "do perform echo(x) as Any return null"),
        register("pkg.noop", &[], "return null"),
    ]);
    out
}

fn base() -> Drive {
    Drive {
        probes: packages,
        reopen_delivered: true,
        ..Drive::new(u64::MAX, false)
    }
}

/// The site of statement `index` of `main`.
fn main_at(index: u32) -> Site {
    Site::new(Unit::Main, [index])
}

/// The site of statement `index` of the declared function `name`.
fn function_at(name: &str, index: u32) -> Site {
    Site::new(Unit::Function(Name::new(name)), [index])
}

/// Runs `text` everywhere under `drive`, delivering by `order`: the
/// uninterrupted run's trace, which every other run left too.
fn traced(drive: Drive, text: &str, order: &[&str]) -> Trace {
    everywhere(drive, text, order).0
}

fn finished(trace: &Trace, expected: &Datum) {
    assert_eq!(trace.end, format!("Finished({expected:?})"), "{trace:?}");
}

/// The A6 cases' document for (a), (b) and (i): a statement perform, one
/// behind a library call and one nested in a body's expression, each
/// entered from a statement of `main`.
const ENTERED: &str = "main { let a = invoke pkg.fetch(\"a\")\n\
    let b = invoke pkg.wrap(\"b\")\n\
    let c = [invoke pkg.fetch_nested(\"c\")]\n\
    return [a, b, c[0]] }\n";

/// (a) and (b): body performs run as their statement's; a request's
/// origin is the `main` statement that entered the library, and a run
/// sealed after each delivery, before consumption, is charged once.
#[test]
fn body_performs_resume_exactly_with_their_entering_statement_as_origin() {
    let trace = traced(base(), ENTERED, &["a", "b", "c"]);
    finished(&trace, &list(&[text("a"), text("b"), text("c")]));
    assert_eq!(trace.origins, [main_at(0), main_at(1), main_at(2)]);
}

/// (c): a body's `then yield` perform, its value awaited through a policy
/// that takes one turn, beside a task that prints when it runs.
#[test]
fn a_then_yield_body_perform_awaited_by_a_policy_resumes_exactly() {
    const SOURCE: &str = "fn tick() { print \"tick\" return 0 }\n\
        main { let h = spawn call tick()\n\
        let p = invoke pkg.fetch_yield(\"c\")\n\
        print \"after\"\n\
        let v = await p via pkg.await\n\
        let d = join h\n\
        return v }\n";
    let trace = traced(base(), SOURCE, &["c"]);
    finished(&trace, &text("c"));
    assert_eq!(trace.origins, [main_at(1)]);
}

/// (d): two iterations of a body's loop perform at one site, told apart by
/// their loop coordinates, each under the statement that entered the loop.
#[test]
fn a_body_loop_performs_once_per_iteration() {
    const SOURCE: &str = "main { let r = invoke pkg.each([\"d0\", \"d1\"]) return r }\n";
    let trace = traced(base(), SOURCE, &["d0", "d1"]);
    finished(&trace, &list(&[text("d0"), text("d1")]));
    assert_eq!(trace.origins, [main_at(0), main_at(0)]);
    assert_ne!(trace.requests[0], trace.requests[1]);
}

/// (e): a task cancelled while its library body waits runs the body's
/// `finally`, which waits in turn; both waits are the declared function's
/// statement's.
#[test]
fn a_cancelled_task_runs_a_waiting_body_finally() {
    const SOURCE: &str = "fn work(x) { let v = invoke pkg.guarded(x) return v }\n\
        main { let h = spawn call work(\"e\")\n\
        do cancel h\n\
        let r = \"none\"\n\
        try { let v = join h\n\
        set r = v } catch e { set r = e.kind }\n\
        return r }\n";
    let trace = traced(base(), SOURCE, &["cleanup"]);
    finished(&trace, &text("cancelled"));
    assert!(!trace.origins.is_empty(), "{trace:?}");
    assert!(
        trace
            .origins
            .iter()
            .all(|origin| *origin == function_at("work", 0)),
        "{trace:?}"
    );
}

/// (f): a run cancelled while a body waits ends cancelled, and its
/// `finally` runs no cleanup.
#[test]
fn a_run_cancelled_in_a_body_wait_runs_no_cleanup() {
    const SOURCE: &str = "main { let v = invoke pkg.guarded(\"f\") return v }\n";
    let trace = traced(
        Drive {
            cancel_at: Some(1),
            ..base()
        },
        SOURCE,
        &[],
    );
    assert_eq!(trace.end, format!("{:?}", End::Cancelled));
    assert_eq!(trace.origins, [main_at(0)]);
}

/// (g): a task a library body spawns runs no document code; its perform's
/// origin is the statement that entered the spawning body.
#[test]
fn a_library_spawned_task_performs_under_its_spawners_statement() {
    const SOURCE: &str = "main { let h = invoke pkg.start(\"g\")\n\
        print \"spawned\"\n\
        let v = join h\n\
        return v }\n";
    let trace = traced(base(), SOURCE, &["g"]);
    finished(&trace, &text("g"));
    assert_eq!(trace.origins, [main_at(0)]);
}

/// (h): each activation of a recursive declared function performs under
/// its own statement.
#[test]
fn a_recursive_declared_call_performs_under_each_activations_statement() {
    const SOURCE: &str = "fn rec(n) { let v = invoke pkg.fetch(n)\n\
        if num.lt(0, n) { let r = call rec(num.add(n, -1)) return [v, r] }\n\
        return [v] }\n\
        main { let r = call rec(1) return r }\n";
    let trace = traced(base(), SOURCE, &["echo", "echo"]);
    finished(&trace, &list(&[int(1), list(&[int(0)])]));
    assert_eq!(
        trace.origins,
        [function_at("rec", 0), function_at("rec", 0)]
    );
    assert_ne!(trace.requests[0], trace.requests[1]);
}

/// (i): body performs bound as reads are answered inside their statement,
/// each handed the statement that entered the library, and charged exactly
/// as the recorded performs are (`K-EFF-012`).
#[test]
fn read_bound_body_performs_carry_their_origin_and_the_recorded_charge() {
    let reads = traced(
        Drive {
            reads: true,
            ..base()
        },
        ENTERED,
        &[],
    );
    finished(&reads, &list(&[text("a"), text("b"), text("c")]));
    assert!(reads.requests.is_empty(), "{reads:?}");
    assert_eq!(reads.origins, [main_at(0), main_at(1), main_at(2)]);
    let recorded = traced(base(), ENTERED, &["a", "b", "c"]);
    assert_eq!(reads.charged, recorded.charged);
}

/// Runs `text` to its end, answering each request with `outcome`, and,
/// when `seal_delivered`, sealing and opening the run cold after each
/// delivery before it runs on: its charge and every request it made.
fn charged(text: &str, outcome: &Outcome, seal_delivered: bool) -> (u64, Vec<Request>) {
    let mut run = Embedder::with(
        text,
        Setup {
            probes: Some(packages),
            ..Setup::default()
        },
    );
    loop {
        match run.run() {
            Step::Ended(End::Finished(_)) => break,
            Step::Ended(other) => panic!("{other:?}: {text}"),
            Step::Slice => {}
            Step::Parked(_) => {
                for request in std::mem::take(&mut run.pending) {
                    run.machine
                        .deliver(wait_of(&request), outcome.clone())
                        .unwrap();
                }
                if seal_delivered {
                    run.reopen(crate::Layout::default(), true);
                }
            }
        }
    }
    let charge = run.machine.meters().charged.expect("exact charge");
    (charge, run.requests)
}

/// K-CHG-007 (`K-CHG-002`): a body's perform is charged outside its
/// formula, once, sealed between delivery and consumption or not. Against
/// a body that performs nothing under the same formula it pays one unit
/// for its atom, the deep size of its argument's copy-out, and the result
/// it consumes, even one it drops, by the measure a document's `perform`
/// consumes by; a function crossing in its argument pays its canonical
/// bytes besides.
#[test]
fn a_body_perform_pays_its_boundary_once() {
    let call = |function: &str, argument: &str| {
        format!(
            "main {{ let f = fn(x) start (x: Any) -> Any bare direct {{ return x }}\n\
             do invoke {function}({argument}) return 0 }}\n"
        )
    };
    let document = "main { do perform echo(\"s\") as Any return 0 }\n";
    let large_result = Outcome::Completed(Datum::Text("x".repeat(1000)));
    let null_result = Outcome::Completed(Datum::Null);
    for sealed in [false, true] {
        let (noop, _) = charged(&call("pkg.noop", "\"s\""), &null_result, sealed);
        // Its atom (1) and the copy-out of "s" (2); consuming a null
        // result costs nothing, as in document code.
        let (small, _) = charged(&call("pkg.discard", "\"s\""), &null_result, sealed);
        assert_eq!(small, noop + 1 + 2, "sealed after delivery: {sealed}");
        // A dropped result of 1000 bytes is consumed for each of them, as a
        // document's `perform` consumes it.
        let (dropped, _) = charged(&call("pkg.discard", "\"s\""), &large_result, sealed);
        assert_eq!(dropped, small + 1000, "sealed after delivery: {sealed}");
        let (in_document, _) = charged(document, &null_result, sealed);
        let (large_in_document, _) = charged(document, &large_result, sealed);
        assert_eq!(dropped - small, large_in_document - in_document);
        // The closure copies out at size 1, as null does, and its function
        // value's canonical bytes are charged besides.
        let (null, _) = charged(&call("pkg.discard", "null"), &null_result, sealed);
        assert_eq!(null, noop + 1 + 1, "sealed after delivery: {sealed}");
        let (crossed, requests) = charged(&call("pkg.discard", "f"), &null_result, sealed);
        let [Request::Effect(request)] = requests.as_slice() else {
            panic!("{requests:?}");
        };
        let [Datum::FunctionValue(value)] = request.args.as_slice() else {
            panic!("{:?}", request.args);
        };
        let bytes = value.canonical_tagged_bytes().unwrap().len() as u64;
        assert!(bytes > 0);
        assert_eq!(crossed, null + bytes, "sealed after delivery: {sealed}");
    }
}
