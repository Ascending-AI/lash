//! PLAN-INDEP over the FIG-6372 corpus (FIG-6440, `K-LIB-006`,
//! `K-CHG-007`): which path answers an operation is invisible to a run.
#![expect(
    clippy::unwrap_used,
    reason = "integration law setup treats malformed corpus and machine failures as failed laws"
)]

#[expect(
    dead_code,
    reason = "the adapter only needs the ordinary start fixture"
)]
#[path = "../src/tests/pages.rs"]
mod pages;

#[expect(
    dead_code,
    reason = "these laws run lowered programs, not the census or the fuzzer's entry"
)]
#[path = "../shared/helper_differential/runner.rs"]
mod runner;

use lash_kernel_vm::Layout;
use runner::{Case, Lowered, Runner};

/// Every corpus case of `dialect` whose index has `parity` gives the same
/// result and charge with its operations' natives answering through their
/// prepared entries (layout 0), uninterrupted, as when it starts with every
/// native declining to its kernel body (a layout of 3 modulo 4) and is
/// sealed and opened cold at each park, by turns, with natives answering
/// through the general call (an odd layout), through prepared entries, and
/// declining again.
/// Each layout shares its prepared library across cases and cold opens;
/// the machine and its heap access are fresh at every open.
fn runs_alike_whichever_path_answers(dialect: &str, parity: Option<usize>) {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("helper_differential/cases.json")).unwrap();
    let runner = Runner::new(dialect).unwrap();
    let plan = [Layout(3), Layout(1), Layout(0)];
    let mut compared = 0;
    for (index, case) in cases
        .iter()
        .filter(|case| case.dialect == dialect)
        .enumerate()
    {
        if parity.is_some_and(|parity| index % 2 != parity) {
            continue;
        }
        let Lowered::Program(program) = runner.lower(case).unwrap() else {
            continue;
        };
        let observe = |layouts: &[Layout]| {
            runner
                .execute(case, &program, layouts)
                .map(|observed| (observed.result, observed.charged))
        };
        assert_eq!(observe(&plan), observe(&[Layout(0)]), "{}", case.id);
        compared += 1;
    }
    assert!(compared > 0, "the corpus must not be empty");
    println!("{dialect}: plan-independence comparisons={compared}");
}

#[test]
fn typescript_even_cases_run_alike_whichever_path_answers() {
    runs_alike_whichever_path_answers("typescript", Some(0));
}

#[test]
fn typescript_odd_cases_run_alike_whichever_path_answers() {
    runs_alike_whichever_path_answers("typescript", Some(1));
}

#[test]
fn python_cases_run_alike_whichever_path_answers() {
    runs_alike_whichever_path_answers("python", None);
}

/// PLAN-INDEP for FIG-6640: numeric native guards decline coercions, a list
/// cursor maps holes and hands arbitrary values through unchanged, while
/// text, Map and Set cursors keep the complete item body and machine deref.
#[test]
fn numeric_and_list_operations_run_alike_after_guard_misses() {
    let runner = Runner::new("typescript").unwrap();
    let sources = [
        "const marks = []; const box = {valueOf() { marks.push('valueOf'); return 7; }}; await observe([10 - 3, 10 - box, Number(box), marks.join(',')]);",
        "const out = []; for (const x of [1, , 3]) { out.push(x === undefined ? 'hole' : x); await observe(x); } const xs: number[] = 'ab' as any; for (const x of xs) { out.push(x); } await observe(out);",
        "const key = {value: 1}; const entriesMap = new Map([[key, 'entry']]); for (const [entryKey, v] of entriesMap) { await observe(v); await observe(entryKey === key); } const membersSet = new Set([key]); for (const entryKey of membersSet) { await observe(entryKey === key); }",
    ];
    for (index, source) in sources.into_iter().enumerate() {
        let case = Case {
            id: format!("FIG-6640-{index}"),
            dialect: "typescript".into(),
            source: source.into(),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        };
        let program = match runner.lower(&case).unwrap() {
            Lowered::Program(program) => program,
            Lowered::Refused(observed) => panic!("{} must lower: {:?}", case.id, observed.result),
        };
        let reference = runner.execute(&case, &program, &[Layout(0)]).unwrap();
        assert_eq!(
            reference.result["type"], "observations",
            "{}: {:?}",
            case.id, reference.result
        );
        for plan in [
            &[Layout(1)][..],
            &[Layout(2)],
            &[Layout(3)],
            &[Layout(0x5eed)],
            &[Layout(0), Layout(2), Layout(3), Layout(1), Layout(0x5eed)],
        ] {
            let observed = runner.execute(&case, &program, plan).unwrap();
            assert_eq!(
                (&observed.result, observed.charged),
                (&reference.result, reference.charged),
                "{}: {plan:?}",
                case.id
            );
        }
    }
}

/// GET-CHARGE and PLAN-INDEP for the scoped operation tariff (FIG-6648).
mod get_contract {
    use lash_kernel_dialect::NamedLibrary;
    use lash_kernel_doc::{
        Datum, ErrorDatum, FunctionRegistry, Handle, Integer, Timestamp, parse_document,
    };
    use lash_kernel_vm::{
        Bounds, CellStart, End, Host, Layout, Machine, PreparedLibrary, Program, Step, Target,
    };
    use std::sync::Arc;

    const ROOMY: Bounds = Bounds {
        charge: 50_000_000,
        memory: 64 << 20,
        call_depth: 200,
        live_tasks: 10,
        requests_per_park: 10,
        join_members: 10,
    };

    fn registry(native: bool) -> Arc<FunctionRegistry> {
        registry_for(native, false)
    }

    fn registry_for(native: bool, python: bool) -> Arc<FunctionRegistry> {
        let mut registry = FunctionRegistry::new();
        lash_kernel_vm::register_machine_functions(&mut registry).unwrap();
        lash_kernel_lib::register_numbers(&mut registry).unwrap();
        lash_kernel_lib::register_collections(&mut registry).unwrap();
        lash_kernel_lib::register_text_json(&mut registry).unwrap();
        lash_ext_date_ecma::register(&mut registry).unwrap();
        lash_ext_url_whatwg::register(&mut registry).unwrap();
        lash_ext_regex_ecma::register(
            &mut registry,
            &Arc::new(lash_ext_regex_ecma::Engine::new(32)),
        )
        .unwrap();
        let mut library = NamedLibrary::from_registry(&registry).unwrap();
        let definitions = if python {
            lash_dialect_python::define_helpers(&mut library).unwrap()
        } else {
            lash_dialect_typescript::define_helpers(&mut library).unwrap()
        };
        let helpers: Vec<_> = definitions
            .into_iter()
            .map(|definition| registry.register(definition, None).unwrap())
            .collect();
        if native {
            if python {
                lash_ext_py_core::implement(&mut registry, helpers).unwrap();
            } else {
                lash_ext_ts_core::implement(&mut registry, helpers).unwrap();
            }
        }
        // Fresh compilation on each cold open must not compile unrelated
        // dialect helpers. Keep the complete reachable graph, plus kernel
        // and extension primitives used directly by these kernel programs.
        fn keep(
            source: &FunctionRegistry,
            target: &mut FunctionRegistry,
            id: lash_kernel_doc::FunctionId,
        ) {
            if target.get(&id).is_some() {
                return;
            }
            let held = source.get(&id).unwrap();
            for callee in held.callees() {
                keep(source, target, *callee);
            }
            target
                .register(held.definition().as_ref().clone(), held.native().cloned())
                .unwrap();
        }
        let mut selected = FunctionRegistry::new();
        for (id, held) in registry.iter() {
            let name = held.definition().name.as_str();
            if (!name.starts_with("ts.") && !name.starts_with("py."))
                || if python {
                    lash_ext_py_core::OPERATIONS.contains(&name)
                } else {
                    lash_ext_ts_core::OPERATIONS.contains(&name)
                }
            {
                keep(&registry, &mut selected, *id);
            }
        }
        Arc::new(selected)
    }

    #[derive(Default)]
    struct Quiet;
    impl Host for Quiet {
        fn clock(&mut self) -> Timestamp {
            Timestamp {
                nanoseconds: Integer::from(0),
            }
        }
        fn random(&mut self) -> u64 {
            0
        }
        fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
            panic!("no reads")
        }
        fn print(&mut self, _: &Datum) {
            panic!("no prints")
        }
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }

    fn run(
        registry: &Arc<FunctionRegistry>,
        body: &str,
        layout: Layout,
        bounds: Bounds,
    ) -> (String, u64) {
        let uses: String = registry
            .iter()
            .map(|(id, held)| format!("use {} = @{id}\n", held.definition().name))
            .collect();
        let numbers = if registry
            .iter()
            .any(|(_, held)| held.definition().name.as_str() == "py.truth")
        {
            "numbers by_spelling\n"
        } else {
            "numbers float\n"
        };
        let mut document =
            parse_document(&format!("kernel 1\n{numbers}{uses}\nmain {{ {body} }}")).unwrap();
        document.manifest.functions =
            lash_kernel_check::requirements(&document, registry.as_ref()).functions;
        let program = Program {
            release: crate::pages::RELEASE,
            document: Arc::new(document),
            library: PreparedLibrary::with_layout(Arc::clone(registry), layout),
        };
        let machine = crate::pages::start_with_layout(
            program,
            bounds,
            CellStart {
                target: Target::Main,
                args: vec![],
                session: None,
                owner: crate::pages::OWNER,
                modes: Default::default(),
                host: vec![],
            },
            layout,
        );
        let mut machine = match machine {
            Ok(machine) => machine,
            Err(error) => return (format!("start {error:?}"), 0),
        };
        let end = match machine.run(&mut Quiet, u64::MAX).unwrap() {
            Step::Ended(End::Finished(finished)) => format!("finished {:?}", finished.result),
            Step::Ended(other) => format!("{other:?}"),
            other => panic!("unexpected {other:?}"),
        };
        (end, machine.meters().charged)
    }

    /// GET-CHARGE: paired equal-vector receivers distinguish ordinary field
    /// reads from regex escaping without measuring the field or its receiver.
    #[test]
    fn plain_reads_cost_twelve_and_dynamic_reads_charge_their_work() {
        let registries = [registry(true), registry(false)];
        for h in [1, 64, 4096] {
            for (plan, registry) in registries.iter().enumerate() {
                for layout in [Layout(0), Layout(1), Layout(2), Layout(3), Layout(0x5eed)] {
                    let setup = format!(
                        r#"let r = {{__ts_brand: null, __ts_type: "brand:regex.ecma", pattern: "{0}", flags: "", lastIndex: 0.0, source: "{0}"}}"#,
                        "a".repeat(h)
                    );
                    let base = run(
                        registry,
                        &format!("{setup} let out = r.source return out"),
                        layout,
                        ROOMY,
                    );
                    let plain = run(
                        registry,
                        &format!("{setup} let out = invoke ts.get(r, \"source\") return out"),
                        layout,
                        ROOMY,
                    );
                    assert_eq!(plain.0, base.0);
                    assert_eq!(plain.1 - base.1, 12, "H={h}, plan={plan}, {layout:?}");
                    let branded = setup.replace("__ts_brand: null", "__ts_brand: ()");
                    let dynamic = run(
                        registry,
                        &format!("{branded} let out = invoke ts.get(r, \"source\") return out"),
                        layout,
                        ROOMY,
                    );
                    assert_eq!(dynamic.0, plain.0);
                    assert!(
                        dynamic.1 >= base.1 + 12 + (2 * h + 10) as u64,
                        "escaping is charged: {dynamic:?}"
                    );
                }
            }
        }
    }

    /// PLAN-INDEP / K-BND-002: an answering native and a native decline
    /// must agree at a call-depth boundary, as well as away from it.
    #[test]
    fn plain_reads_have_the_same_call_depth_refusal_under_every_plan() {
        let registries = [registry(true), registry(false)];
        let body = "let r = {value: 7.0} let probe_get = fn(r) { let out = invoke ts.get(r, \"value\") return out } let out = apply probe_get(r) return out";
        for depth in 1..=4 {
            let bounds = Bounds {
                call_depth: depth,
                ..ROOMY
            };
            let reference = run(&registries[0], body, Layout(0), bounds);
            for registry in &registries {
                for layout in [Layout(0), Layout(1), Layout(2), Layout(3), Layout(0x5eed)] {
                    assert_eq!(
                        run(registry, body, layout, bounds),
                        reference,
                        "depth={depth}, {layout:?}"
                    );
                }
            }
        }
    }

    #[derive(Default)]
    struct TraceHost {
        printed: Vec<Datum>,
    }
    impl Host for TraceHost {
        fn clock(&mut self) -> Timestamp {
            Timestamp {
                nanoseconds: Integer::from(0),
            }
        }
        fn random(&mut self) -> u64 {
            0
        }
        fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
            panic!("no reads")
        }
        fn print(&mut self, value: &Datum) {
            self.printed.push(value.clone());
        }
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }

    // Compare semantic sites and pending payload bytes, resolving the site
    // pages. Ready order and every frame's continuation are part of the oracle.
    fn canonical(
        root: &lash_kernel_state::paged::CheckpointRoot,
        source: &crate::pages::Source,
    ) -> Vec<Vec<u8>> {
        use lash_kernel_doc::graph_tokens::encode_body;
        use lash_kernel_state::paged::tree::Tree;
        use lash_kernel_state::paged::{EditEpoch, PagedHeap, TaskRecord};
        let heap =
            PagedHeap::open_header(root.heap, Box::new(source.clone()), EditEpoch(2)).unwrap();
        let access = heap.pages();
        let mut rows = Vec::new();
        Tree::<lash_kernel_doc::TaskId>::of_sequence(&root.ready)
            .visit(access, &mut |_, id| {
                rows.push(encode_body(id));
                true
            })
            .unwrap();
        Tree::<TaskRecord>::of_keyed(&root.tasks)
            .visit(access, &mut |key, task| {
                for index in 0..task.frames.len() {
                    let (_, frame) = task.frames.rank_get(index, access).unwrap().unwrap();
                    let site = frame.statement.resolve(access).unwrap();
                    let pending = frame.pending.as_ref().map(encode_body);
                    let mut loops = Vec::new();
                    for at in 0..frame.loops.len() {
                        let (_, row) = frame.loops.rank_get(at, access).unwrap().unwrap();
                        loops.push(encode_body(&row));
                    }
                    rows.push(
                        serde_json::to_vec(&(key, index, site.as_ref(), pending, loops)).unwrap(),
                    );
                }
                true
            })
            .unwrap();
        rows
    }

    type Trace = (
        String,
        Vec<Datum>,
        Vec<lash_kernel_vm::Request>,
        u64,
        Vec<Vec<Vec<u8>>>,
    );

    fn trace(
        registry: &Arc<FunctionRegistry>,
        source_text: &str,
        layout: Layout,
        bounds: Bounds,
        cold: bool,
    ) -> Trace {
        use lash_kernel_vm::{Outcome, Request};
        let uses: String = registry
            .iter()
            .map(|(id, held)| format!("use {} = @{id}\n", held.definition().name))
            .collect();
        let numbers = if registry
            .iter()
            .any(|(_, held)| held.definition().name.as_str() == "py.truth")
        {
            "numbers by_spelling\n"
        } else {
            "numbers float\n"
        };
        let mut document = parse_document(&format!(
            "kernel 1\n{numbers}effect echo(x: Any) -> Any\n{uses}\n{source_text}"
        ))
        .unwrap();
        document.manifest.functions =
            lash_kernel_check::requirements(&document, registry.as_ref()).functions;
        let document = Arc::new(document);
        let program_in = |layout| Program {
            release: crate::pages::RELEASE,
            document: Arc::clone(&document),
            library: PreparedLibrary::with_layout(Arc::clone(registry), layout),
        };
        let source = crate::pages::Source::default();
        let machine = crate::pages::with_source(
            program_in(layout),
            bounds,
            CellStart {
                target: Target::Main,
                args: vec![],
                session: None,
                owner: crate::pages::OWNER,
                modes: Default::default(),
                host: vec![],
            },
            layout,
            &source,
        );
        let mut machine = match machine {
            Ok(machine) => machine,
            Err(error) => return (format!("start {error:?}"), vec![], vec![], 0, vec![]),
        };
        let mut host = TraceHost::default();
        let mut requests = Vec::new();
        let mut continuations = Vec::new();
        let mut plans = [Layout(3), Layout(0), Layout(2), Layout(1), Layout(0x5eed)]
            .into_iter()
            .cycle();
        let end = loop {
            let step = machine
                .run(&mut host, if cold { 1 } else { u64::MAX })
                .unwrap();
            if let Step::Ended(end) = step {
                break match end {
                    End::Finished(finished) => format!("finished {:?}", finished.result),
                    other => format!("{other:?}"),
                };
            }
            if matches!(step, Step::Parked(_)) {
                let sealed = machine.seal(crate::pages::BUDGET).unwrap().unwrap();
                continuations.push(canonical(&sealed.root, &source));
            }
            if cold {
                let layout = plans.next().unwrap();
                crate::pages::reopen(&mut machine, &program_in(layout), bounds, &source, layout);
            }
            if let Step::Parked(park) = step {
                for request in park.requests {
                    requests.push(request.clone());
                    let Request::Effect(effect) = request else {
                        panic!("only echo")
                    };
                    machine
                        .deliver(effect.wait, Outcome::Completed(effect.args[0].clone()))
                        .unwrap();
                }
            }
        };
        (
            end,
            host.printed,
            requests,
            machine.meters().charged,
            continuations,
        )
    }

    /// GET-ORDER / CONT-ONCE / PLAN-INDEP: key conversion mutates the
    /// receiver, counts once, parks, and can raise. Other tasks retain their
    /// effect identities and ready order under every cold fallback cut.
    #[test]
    fn dynamic_reads_preserve_order_and_continuations_across_plans() {
        let registries = [registry(true), registry(false)];
        let program = r#"
fn peer() { let reply = perform echo("peer") as Any return reply }
main {
  let peer_task = spawn call peer()
  let target = {value: 1.0}
  let count = 0.0
  let key = {toString: fn(this, args) {
    set count = num.add(count, 1.0)
    set target.value = 7.0
    let name = perform echo("value") as Any
    return name
  }}
  let value = ts.get(target, key)
  print (value, count)
  let regex = {__ts_brand: (), __ts_type: "brand:regex.ecma", pattern: "a/b", flags: "g", lastIndex: 0.0}
  print (ts.get(regex, "source"), ts.get(regex, "flags"))
  let holes = [(), 3.0]
  print (ts.get(holes, 0.0), ts.get("abc", "length"), ts.get(target, "missing"))
  let bad_key = {toString: fn(this, args) {
    let reply = perform echo("raise") as Any
    throw error.new("type_error", reply, null)
  }}
  try { let ignored = ts.get(target, bad_key) } catch err { print err.message }
  try { let ignored = ts.get(null, "value") } catch err { print err.kind }
  try { let ignored = ts.get("😀", "0") } catch err { print err.kind }
  let joined = join peer_task
  let last = perform echo("done") as Any
  return (value, count, last)
}
"#;
        for (source, bounds) in [
            (program.to_owned(), ROOMY),
            (
                program.replace(
                    "set target.value = 7.0",
                    "set target.value = text.repeat(\"a\", 1000000)",
                ),
                Bounds {
                    memory: 1 << 18,
                    ..ROOMY
                },
            ),
            (
                program.to_owned(),
                Bounds {
                    charge: 100,
                    ..ROOMY
                },
            ),
            (
                program.to_owned(),
                Bounds {
                    call_depth: 4,
                    ..ROOMY
                },
            ),
        ] {
            let reference = trace(&registries[0], &source, Layout(0), bounds, false);
            if bounds == ROOMY {
                assert_eq!(
                    reference.0,
                    "finished Tuple([Float(Float(7.0)), Float(Float(1.0)), Text(\"done\")])"
                );
                assert_eq!(reference.2.len(), 4);
            }
            for registry in &registries {
                for layout in [Layout(0), Layout(1), Layout(2), Layout(3), Layout(0x5eed)] {
                    assert_eq!(
                        trace(registry, &source, layout, bounds, false),
                        reference,
                        "warm {bounds:?} {layout:?}"
                    );
                    assert_eq!(
                        trace(registry, &source, layout, bounds, true),
                        reference,
                        "cold {bounds:?} {layout:?}"
                    );
                }
            }
        }
    }

    /// GET-CHARGE: unrelated cycles and large values remain existing fields.
    /// Kir's per-site hint checks names after changing records and keys.
    #[test]
    fn plain_reads_do_not_walk_graphs_and_hints_verify_names() {
        let registries = [registry(true), registry(false)];
        let setup = r#"let cycle = {} set cycle.self = cycle
          let big = text.repeat("a", 100000)
          let target = {unrelated: cycle, value: big}
          let other = {value: 9.0, unrelated: cycle}
          let out = []
          for r in [target, other, target] {
            for key in ["value", "unrelated", "missing"] {
              let value = ts.get(r, key)
              set out[list.len(out)] = same(value, r[key])
            }
          }
          return out"#;
        let reference = run(&registries[0], setup, Layout(0), ROOMY);
        assert_eq!(
            reference.0,
            "finished List([Bool(true), Bool(true), Bool(true), Bool(true), Bool(true), Bool(true), Bool(true), Bool(true), Bool(true)])"
        );
        for registry in &registries {
            for layout in [Layout(0), Layout(1), Layout(2), Layout(3), Layout(0x5eed)] {
                assert_eq!(run(registry, setup, layout, ROOMY), reference);
                let baseline = run(
                    registry,
                    "let big = text.repeat(\"a\", 100000) let r = {value: big} let out = r.value return out",
                    layout,
                    ROOMY,
                );
                let read = run(
                    registry,
                    "let big = text.repeat(\"a\", 100000) let r = {value: big} let out = invoke ts.get(r, \"value\") return out",
                    layout,
                    ROOMY,
                );
                assert_eq!(read.0, baseline.0);
                assert_eq!(read.1 - baseline.1, 12);
            }
        }
    }

    /// ROOTS-CANON: inspect the actual memory threshold rather than choosing
    /// a roomy bound that hides the operation's frame holdings.
    #[test]
    fn a_plain_read_refuses_memory_at_the_same_boundary_under_every_plan() {
        let registries = [registry(true), registry(false)];
        for body in [
            "let r = {value: 7.0} let out = ts.get(r, \"value\") return out",
            "let big = text.repeat(\"a\", 1024) let r = {value: big} let out = ts.get(r, \"value\") return out",
            "let r = {value: {x: 7.0}} set r.self = r let out = ts.get(r, \"value\") return out",
        ] {
            let mut low = 1;
            let mut high = 65536;
            while low < high {
                let mid = (low + high) / 2;
                let observed = run(
                    &registries[0],
                    body,
                    Layout(0),
                    Bounds {
                        memory: mid,
                        ..ROOMY
                    },
                );
                if observed.0.starts_with("finished") {
                    high = mid
                } else {
                    low = mid + 1
                }
            }
            for memory in [low - 1, low, low + 64, low + 512] {
                let bounds = Bounds { memory, ..ROOMY };
                let reference = trace(
                    &registries[0],
                    &format!("main {{ {body} }}"),
                    Layout(0),
                    bounds,
                    false,
                );
                for cold in [false, true] {
                    for registry in &registries {
                        for layout in [Layout(0), Layout(1), Layout(2), Layout(3), Layout(0x5eed)] {
                            assert_eq!(
                                trace(
                                    registry,
                                    &format!("main {{ {body} }}"),
                                    layout,
                                    bounds,
                                    cold
                                ),
                                reference,
                                "cold={cold}, memory={memory}, {layout:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// K-BND-002: the registered P2-6a/6b operations have the same depth
    /// budget on their answering primitive paths as when every native misses.
    #[test]
    fn existing_typescript_operations_have_plan_independent_depth() {
        let registries = [registry(true), registry(false)];
        let expressions = [
            "ts.to_boolean(true)",
            "ts.add(1.0, 2.0)",
            "ts.add(\"a\", \"b\")",
            "ts.sub(1.0, 2.0)",
            "ts.to_number(1.0)",
            "ts.for_of.item([], 1.0)",
            "ts.lt(1.0, 2.0)",
            "ts.le(1.0, 2.0)",
            "ts.gt(1.0, 2.0)",
            "ts.ge(1.0, 2.0)",
            "ts.lt(\"a\", \"b\")",
            "ts.le(\"a\", \"b\")",
            "ts.gt(\"a\", \"b\")",
            "ts.ge(\"a\", \"b\")",
        ];
        for expression in expressions {
            let body = format!(
                "let probe = fn() {{ return {expression} }} let out = apply probe() return out"
            );
            for depth in 1..=5 {
                let bounds = Bounds {
                    call_depth: depth,
                    ..ROOMY
                };
                let reference = run(&registries[0], &body, Layout(0), bounds);
                for registry in &registries {
                    for layout in [Layout(0), Layout(1), Layout(2), Layout(3)] {
                        assert_eq!(
                            run(registry, &body, layout, bounds),
                            reference,
                            "{expression}, depth={depth}, {layout:?}"
                        );
                    }
                }
            }
        }
    }

    /// K-BND operation admission: the older registered native domains must
    /// obey the same memory verdict, including the charge at refusal.
    #[test]
    fn existing_typescript_operations_have_plan_independent_memory() {
        let registries = [registry(true), registry(false)];
        for expression in [
            "ts.to_boolean(true)",
            "ts.to_boolean(0.0)",
            "ts.to_boolean(\"x\")",
            "ts.to_boolean(null)",
            "ts.to_boolean(())",
            "ts.to_boolean({})",
            "ts.add(1.0, 2.0)",
            "ts.add(float.to_int(1.0), 2.0)",
            "ts.add(1.0, float.to_int(2.0))",
            "ts.add(\"a\", \"b\")",
            "ts.sub(1.0, 2.0)",
            "ts.to_number(1.0)",
            "ts.for_of.item([], 1.0)",
            "ts.lt(1.0, 2.0)",
            "ts.le(1.0, 2.0)",
            "ts.gt(1.0, 2.0)",
            "ts.ge(1.0, 2.0)",
            "ts.lt(\"a\", \"b\")",
            "ts.le(\"a\", \"b\")",
            "ts.gt(\"a\", \"b\")",
            "ts.ge(\"a\", \"b\")",
        ] {
            assert_memory_admission(&registries, expression);
        }
    }

    /// K-BND-002: both Python operations obey canonical admission too.
    #[test]
    fn existing_python_operations_have_plan_independent_admission() {
        let registries = [registry_for(true, true), registry_for(false, true)];
        for expression in [
            "py.truth(true)",
            "py.truth(1)",
            "py.truth(\"x\")",
            "py.truth([])",
            "py.truth(())",
            "py.add(1, 2)",
            "py.add(1.0, 2.0)",
            "py.add(\"a\", \"b\")",
            "py.add([1], [2])",
            "py.add((1,), (2,))",
        ] {
            let body = format!(
                "let probe = fn() {{ return {expression} }} let out = apply probe() return out"
            );
            for depth in 1..=4 {
                let bounds = Bounds {
                    call_depth: depth,
                    ..ROOMY
                };
                let reference = run(&registries[0], &body, Layout(0), bounds);
                for registry in &registries {
                    for layout in [Layout(0), Layout(1), Layout(2), Layout(3)] {
                        assert_eq!(
                            run(registry, &body, layout, bounds),
                            reference,
                            "{expression}, depth={depth}, {layout:?}"
                        );
                    }
                }
            }
            assert_memory_admission(&registries, expression);
        }
    }

    fn assert_memory_admission(registries: &[Arc<FunctionRegistry>; 2], expression: &str) {
        let body = format!("let out = {expression} return out");
        let mut low = 1;
        let mut high = 65536;
        while low < high {
            let mid = (low + high) / 2;
            if run(
                &registries[0],
                &body,
                Layout(0),
                Bounds {
                    memory: mid,
                    ..ROOMY
                },
            )
            .0
            .starts_with("finished")
            {
                high = mid
            } else {
                low = mid + 1
            }
        }
        for memory in [low - 1, low, low + 64] {
            let bounds = Bounds { memory, ..ROOMY };
            let source = format!("main {{ {body} }}");
            let reference = trace(&registries[0], &source, Layout(0), bounds, false);
            for cold in [false, true] {
                for registry in registries {
                    for layout in [Layout(0), Layout(1), Layout(2), Layout(3)] {
                        assert_eq!(
                            trace(registry, &source, layout, bounds, cold),
                            reference,
                            "{expression}, cold={cold}, memory={memory}, {layout:?}"
                        );
                    }
                }
            }
        }
    }
}
