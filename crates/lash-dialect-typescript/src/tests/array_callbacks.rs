//! The callback methods' list paths (FIG-5886) against the definitions they
//! replaced: `array_callbacks_before.kernel` swapped into the array helpers
//! gives a second library, and every program here runs on both. Results,
//! errors, prints, tool calls and each callback's receiver and arguments, in
//! order, are the same; only the charge, which bounds the work done, drops.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{Environment, Library, NamedLibrary};
use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionCatalog, FunctionId, Handle, Integer, Timestamp, parse_document,
};
use lash_kernel_vm::{
    Bindings, Bounds, End, Host, KernelMachine, Machine, Outcome, PreparedLibrary, Program,
    Request, Start, Step, Target,
};

const BEFORE: &str = include_str!("array_callbacks_before.kernel");

const HELPERS: [&str; 11] = [
    "map",
    "filter",
    "forEach",
    "every",
    "some",
    "find",
    "findIndex",
    "findLast",
    "findLastIndex",
    "reduce",
    "reduceRight",
];

const BOUNDS: Bounds = Bounds {
    charge: 100_000_000,
    memory: 64 << 20,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

struct Kernel {
    library: NamedLibrary,
    prepared: PreparedLibrary,
}

/// The library with the array helpers as `array` defines them.
fn kernel_with(array: Option<&'static str>) -> Kernel {
    let mut registry = super::kernel_registry();
    let mut library = NamedLibrary::from_registry(&registry).unwrap();
    let mut objects = crate::builtins::objects();
    if let Some(array) = array {
        let object = objects
            .iter_mut()
            .find(|object| object.source.contains("function ts.array.map("))
            .unwrap();
        object.source = array;
    }
    for definition in crate::package::define_helpers_from(&mut library, objects).unwrap() {
        registry.register(definition, None).unwrap();
    }
    Kernel {
        library,
        prepared: PreparedLibrary::new(Arc::new(registry)),
    }
}

fn after() -> &'static Kernel {
    static KERNEL: OnceLock<Kernel> = OnceLock::new();
    KERNEL.get_or_init(|| kernel_with(None))
}

fn before() -> &'static Kernel {
    static KERNEL: OnceLock<Kernel> = OnceLock::new();
    KERNEL.get_or_init(|| {
        let source = crate::builtins::objects()
            .into_iter()
            .find(|object| object.source.contains("function ts.array.map("))
            .unwrap()
            .source;
        kernel_with(Some(Box::leak(swapped(source, BEFORE).into_boxed_str())))
    })
}

/// `source` with each function `replacements` defines in place of the one
/// of the same name.
fn swapped(source: &str, replacements: &str) -> String {
    let mut out = source.to_string();
    let mut swaps = 0;
    for chunk in replacements.split("\nfunction ").skip(1) {
        let name = chunk.split('(').next().unwrap();
        let chunk = format!("function {}\n}}\n", chunk.split("\n}\n").next().unwrap());
        let start = out
            .find(&format!("function {name}("))
            .unwrap_or_else(|| panic!("no {name}"));
        let end = start + out[start..].find("\n}\n").unwrap() + 3;
        out.replace_range(start..end, &chunk);
        swaps += 1;
    }
    assert_eq!(swaps, HELPERS.len() + 2);
    out
}

#[derive(Default)]
struct Console {
    printed: Vec<Datum>,
}

impl Host for Console {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "no_projection".into(),
            message: "the law reads no projection".into(),
            data: Datum::Null,
        })
    }
    fn print(&mut self, value: &Datum) {
        self.printed.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// What a run shows: how it ended, what it printed, the tools it called
/// with their arguments and the sleeps it asked for, in order, and its
/// charge.
#[derive(Debug, PartialEq)]
struct Observed {
    end: End,
    printed: Vec<Datum>,
    requests: Vec<(String, Vec<Datum>)>,
    charged: u64,
}

/// Runs `program` to its end, answering each tool call with its argument
/// and each sleep at once. With `relay`, the run returns after every
/// statement and moves to a machine imported afresh from its export, at
/// every slice and every park.
fn observe(program: Program, relay: bool) -> Observed {
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Bindings::default(),
    };
    let mut machine = KernelMachine::start(program.clone(), BOUNDS, start).unwrap();
    let mut console = Console::default();
    let mut requests = Vec::new();
    let slice = if relay { 0 } else { u64::MAX };
    let hop = |machine: &mut KernelMachine| {
        if relay {
            let parked = machine.export().unwrap();
            *machine = KernelMachine::import(program.clone(), BOUNDS, parked).unwrap();
        }
    };
    loop {
        match machine.run(&mut console, slice).unwrap() {
            Step::Ended(end) => {
                return Observed {
                    end,
                    printed: console.printed,
                    requests,
                    charged: machine.meters().charged,
                };
            }
            Step::Slice => hop(&mut machine),
            Step::Parked(park) => {
                hop(&mut machine);
                for request in park.requests {
                    let (wait, outcome) = match request {
                        Request::Sleep(sleep) => {
                            requests.push(("sleep".to_string(), Vec::new()));
                            (sleep.wait, Outcome::Elapsed)
                        }
                        Request::Effect(effect) => {
                            let answer = effect.args.first().cloned().unwrap_or(Datum::Null);
                            requests.push((effect.effect.as_str().to_string(), effect.args));
                            (effect.wait, Outcome::Completed(answer))
                        }
                    };
                    machine.deliver(wait, outcome).unwrap();
                }
                hop(&mut machine);
            }
        }
    }
}

/// Lowers a TypeScript cell against `kernel`'s library.
fn lowered(kernel: &Kernel, source: &str) -> Program {
    let environment = Environment {
        tool_roots: &BTreeSet::new(),
        library: &kernel.library,
        effects: &super::machine::effects(),
        controls: super::controls(),
        bindings: &BTreeSet::new(),
        functions: &BTreeMap::new(),
    };
    let lowered =
        crate::lower(source, &environment).unwrap_or_else(|error| panic!("{error}\n{source}"));
    Program {
        document: Arc::new(lowered.document),
        library: kernel.prepared.clone(),
    }
}

/// How a run ended, without the numbering of the heap objects its
/// bindings hold: a definition that allocates less numbers them otherwise.
fn ending(end: &End) -> Result<(&Datum, bool), &End> {
    match end {
        End::Finished(finished) => Ok((&finished.result, finished.finish)),
        other => Err(other),
    }
}

/// Runs `source` on both libraries and gives the run on the new one, once
/// its results, errors, prints and calls are shown equal to the old one's.
fn same_on_both(source: &str) -> Observed {
    let old = observe(lowered(before(), source), false);
    let new = observe(lowered(after(), source), false);
    assert_eq!(ending(&new.end), ending(&old.end), "{source}");
    assert_eq!(new.printed, old.printed, "{source}");
    assert_eq!(new.requests, old.requests, "{source}");
    new
}

/// The text a cell logs a value as: holes, `undefined` and the receiver
/// apart.
const DESCRIBE: &str = r#"
function describe(v: any): string {
  if (v === undefined) { return "u"; }
  if (v === null) { return "null"; }
  if (Array.isArray(v)) {
    let s = "[";
    for (let i = 0; i < v.length; i++) { s = s + (i in v ? describe(v[i]) : "_") + ","; }
    return s + "]";
  }
  if (typeof v === "object") { return "{" + Object.keys(v).join(",") + "}"; }
  return typeof v + ":" + String(v);
}
"#;

/// The receivers the corpus calls each method on, by scenario.
const MAKE: &str = r#"
function make(s: number): any {
  if (s === 0) { return [1, 2, 3, 4, 5]; }
  if (s === 1) { return [, 1, , 3, , ]; }
  if (s === 2) { return [undefined, null, 0, "", NaN, -0]; }
  if (s === 12) { const x: any[] = []; x.length = 4; x[2] = "two"; return x; }
  if (s === 13) { return {0: "a", 2: "c", length: 3}; }
  if (s === 14) { return "abc"; }
  if (s === 15) { return []; }
  if (s === 16) { return [,]; }
  return [1, 2, 3, 4, 5];
}
"#;

/// What the callback does to the receiver on its `c`th call, by scenario:
/// deletes and writes ahead, shortens, shortens and grows again, grows on
/// every call, shifts and unshifts, empties, throws.
const MUTATE: &str = r#"
function mutate(s: number, c: number, a: any) {
  if (s === 4 && c === 0) { delete a[2]; a[3] = "w"; }
  if (s === 5 && c === 1) { a.length = 2; }
  if (s === 6 && c === 1) { a.length = 1; }
  if (s === 6 && c === 2) { a.push("p", "q", "r", "s"); }
  if (s === 7) { a.push(c); }
  if (s === 8 && c === 0) { a.shift(); }
  if (s === 8 && c === 2) { a.unshift("u"); }
  if (s === 9 && c === 2) { throw new Error("boom" + c); }
  if (s === 10 && c === 0) { a.length = 0; }
  if (s === 11 && c === 0) { delete a[c]; delete a[c + 1]; }
  if (s === 13 && c === 0) { a[1] = "b"; delete a[2]; }
}
"#;

/// A cell that calls `helper` on every scenario's receiver with a callback
/// that logs its receiver and arguments, changes the receiver and answers
/// the next of `returns`, and logs each call's outcome.
fn corpus(helper: &str, returns: &str) -> String {
    let reduce = helper.starts_with("reduce");
    let call = if reduce {
        format!(
            "(s % 2 === 0 ? Array.prototype.{helper}.call(a, cb, \"init\") : Array.prototype.{helper}.call(a, cb))"
        )
    } else {
        format!("Array.prototype.{helper}.call(a, cb, ctx)")
    };
    format!(
        r##"{DESCRIBE}{MAKE}{MUTATE}
const log: string[] = [];
const ctx = {{ tag: "ctx" }};
const returns: any[] = {returns};
for (let s = 0; s < 17; s++) {{
  const a: any = make(s);
  let calls = 0;
  const cb = function() {{
    const c = calls;
    calls = calls + 1;
    let line = "s" + s + "#" + c + " this=" + (this === ctx ? "ctx" : describe(this)) + " n=" + arguments.length;
    for (let i = 0; i < arguments.length; i++) {{
      const v = arguments[i];
      line = line + " " + (v === a ? "A" : describe(v));
    }}
    log.push(line);
    mutate(s, c, a);
    return returns[c % returns.length];
  }};
  let outcome = "";
  try {{ outcome = describe({call}); }} catch (e) {{ outcome = "threw " + String(e); }}
  log.push("s" + s + " out=" + outcome + " a=" + describe(a) + " calls=" + calls);
}}
for (const callback of [undefined, 5, {{}}, "f", Math, String, Boolean]) {{
  try {{ log.push(describe(([1] as any).{helper}(callback))); }} catch (e) {{ log.push("threw " + String(e)); }}
}}
for (const receiver of [null, undefined]) {{
  try {{ log.push(describe(Array.prototype.{helper}.call(receiver, () => true))); }} catch (e) {{ log.push("threw " + String(e)); }}
}}
console.log(log.join("\n"));
await finish(log);
"##
    )
}

/// Each method gives what its replaced definition gave on dense, sparse
/// and generic receivers, under callbacks that delete, write, shorten,
/// regrow and grow the receiver or throw, with answers of every
/// truthiness, with built-in functions as callbacks and with callbacks
/// that are no function.
#[test]
fn callback_methods_match_their_replaced_definitions() {
    let sequences = [
        "[0, \"x\", false, null, {}, NaN, \"\", 1, [], undefined, -0, true]",
        "[false, 0, \"\", null, undefined, NaN]",
        "[true, 1, \"x\", {}, [], -1]",
    ];
    for helper in HELPERS {
        for returns in sequences {
            let observed = same_on_both(&corpus(helper, returns));
            let End::Finished(finished) = &observed.end else {
                panic!("{helper}: {:?}", observed.end);
            };
            assert!(finished.finish, "{helper}");
            assert!(
                matches!(&observed.printed[..], [Datum::Text(text)] if text.contains("s0#0") && text.contains("s1 out=")),
                "{helper}: {:?}",
                observed.printed
            );
        }
    }
}

/// A callback's uncaught throw ends the run with that error, after the
/// same calls; an async callback's waits are the same tool calls, in the
/// same order.
#[test]
fn throwing_and_waiting_callbacks_match_their_replaced_definitions() {
    for helper in HELPERS {
        // An answer that makes no method stop before the throw.
        let answer = helper == "every";
        let source = format!(
            "const a = [1, , 3, 4]; let seen = '';\n\
             a.{helper}(function(x: any, y: any) {{ seen += String(x) + String(y); if (seen.length > 4) {{ throw new TypeError(seen); }} return {answer}; }});\n\
             await finish(seen);"
        );
        let observed = same_on_both(&source);
        assert!(
            matches!(&observed.end, End::Error(lash_kernel_vm::RunError::Uncaught(Datum::Error(error))) if error.kind == "TypeError"),
            "{helper}: {:?}",
            observed.end
        );
        let source = format!(
            "const a: any = [1, , 3, 4]; const log: any[] = [];\n\
             const out = a.{helper}(async function(x: any, y: any) {{ log.push(String(x) + String(y)); await echo(x); log.push(await echo(y)); a.length = 3; return x; }});\n\
             await sleep(5);\n\
             await finish({{ log: log, kind: typeof out }});"
        );
        let observed = same_on_both(&source);
        assert!(
            observed.requests.iter().any(|(name, _)| name == "echo"),
            "{helper}: {:?}",
            observed.requests
        );
    }
}

/// The library functions `roots` name and every one their bodies reach,
/// which a document lists so that a run parked inside any of them resumes.
fn manifest(kernel: &Kernel, roots: &[&str]) -> BTreeMap<FunctionId, String> {
    let mut listed = BTreeMap::new();
    let mut pending: Vec<FunctionId> = roots
        .iter()
        .map(|root| kernel.library.resolve(root).unwrap())
        .collect();
    while let Some(function) = pending.pop() {
        let definition = kernel.library.definition(&function).unwrap();
        if listed
            .insert(function, definition.name.to_string())
            .is_none()
            && let Some(body) = definition.body()
        {
            pending.extend(body.functions.keys().copied());
        }
    }
    listed
}

/// A kernel closure that waits on a tool inside each method parks the run
/// with the method's loop under way. Resumed from an export at every park
/// and after every statement, the run ends as the straight run does, with
/// the same calls and charge, and the straight run answers as the replaced
/// definition's does.
#[test]
fn a_callback_that_parks_inside_a_method_resumes_identically() {
    for helper in HELPERS {
        let name = format!("ts.array.{helper}");
        // An answer that ends no method's loop early.
        let answer = helper == "every";
        let program = |kernel: &Kernel| {
            let mut header =
                String::from("kernel 1\nnumbers by_spelling\neffect echo(x?: Any) -> Any\n");
            for (id, used) in manifest(kernel, &[name.as_str(), "list.len", "same"]) {
                header.push_str(&format!("use {used} = @{id}\n"));
            }
            let text = format!(
                "{header}main {{\n\
                 let log = []\n\
                 let xs = [1, (), 3, 4, 5]\n\
                 let f = fn(receiver, passed) {{\n\
                   let r = perform echo(passed) as Any\n\
                   set log[list.len(log)] = (passed, r)\n\
                   if same(list.len(log), 2) {{ set xs[3] = () }}\n\
                   return {answer}\n\
                 }}\n\
                 let margs = [f]\n\
                 let out = invoke {name}(xs, margs)\n\
                 return (out, log, xs)\n\
                 }}\n"
            );
            let document = parse_document(&text).unwrap_or_else(|error| panic!("{error}\n{text}"));
            Program {
                document: Arc::new(document),
                library: kernel.prepared.clone(),
            }
        };
        let straight = observe(program(after()), false);
        assert!(
            matches!(straight.end, End::Finished(_)),
            "{helper}: {:?}",
            straight.end
        );
        assert!(straight.requests.len() >= 2, "{helper} parks");
        assert_eq!(
            observe(program(after()), true),
            straight,
            "{helper}, relayed"
        );
        let old = observe(program(before()), false);
        assert_eq!(
            (ending(&straight.end), &straight.requests),
            (ending(&old.end), &old.requests),
            "{helper}"
        );
    }
}

/// Direct Map calls are new source documents: every executed form is charged
/// once, and every statement and effect boundary can be exported and resumed
/// without changing values, effects or that charge (K-CHG-007, K-MACH-008).
#[test]
fn direct_map_calls_account_for_their_work_and_resume_at_every_statement() {
    let program = lowered(
        after(),
        r#"
        const m = new Map();
        m.set('key', await echo(7));
        const alias = m;
        const read = () => alias.get('key');
        console.log(read(), alias.has('key'), m.get('missing'));
        const generic = (o, key) => o.get(key);
        console.log(generic(m, await echo('key')));
    "#,
    );
    let mut machine = KernelMachine::start(
        program.clone(),
        BOUNDS,
        Start {
            target: Target::Main,
            args: Vec::new(),
            bindings: Bindings::default(),
        },
    )
    .unwrap();
    machine.count_work();
    let mut console = Console::default();
    loop {
        match machine.run(&mut console, u64::MAX).unwrap() {
            Step::Ended(End::Finished(_)) => break,
            Step::Parked(park) => {
                for request in park.requests {
                    let Request::Effect(effect) = request else {
                        panic!("effect expected")
                    };
                    let answer = effect.args.first().cloned().unwrap_or(Datum::Null);
                    machine
                        .deliver(effect.wait, Outcome::Completed(answer))
                        .unwrap();
                }
            }
            other => panic!("{other:?}"),
        }
    }
    let work = machine.work().unwrap();
    assert_eq!(work.charged, machine.meters().charged);
    assert_eq!(work.residual(), 0);
    assert_eq!(work.unattributed_charge, 0);
    assert_eq!(work.insertion_residual(), 0);
    for name in ["ts.member.get", "ts.member.set", "ts.member.has"] {
        assert!(
            !work.codes.iter().any(|code| code.symbol.name == name),
            "Map calls must directly invoke their helper: {name}"
        );
    }
    let straight = observe(program.clone(), false);
    assert!(matches!(straight.end, End::Finished(_)), "{straight:?}");
    assert_eq!(straight.charged, work.charged);
    assert_eq!(observe(program, true), straight);
}
