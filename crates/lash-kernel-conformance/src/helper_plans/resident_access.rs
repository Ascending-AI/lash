//! K-LIB-006 / K-MACH-002: resident access is a specialization of the
//! same helper image, including its activation and refusal boundaries.

use super::*;
use lash_kernel_vm::KernelMachine;

#[derive(Debug, PartialEq)]
struct AccessObserved {
    end: Result<crate::ExpectedEnd, String>,
    prints: Vec<Datum>,
    stops: Vec<(String, u64)>,
    memory: u64,
}

fn observe(program: &Program, bounds: Bounds, slice: u64, cold: bool) -> AccessObserved {
    let source = crate::pages::Source::default();
    let machine = KernelMachine::start(
        program.clone(),
        bounds,
        CellStart {
            owner: crate::pages::OWNER,
            target: Target::Main,
            args: Vec::new(),
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        },
        Box::new(source.clone()),
    );
    let mut machine = match machine {
        Ok(machine) => machine,
        Err(error) => {
            return AccessObserved {
                end: Err(error.to_string()),
                prints: Vec::new(),
                stops: Vec::new(),
                memory: 0,
            };
        }
    };
    let mut world = World::default();
    let mut stops = Vec::new();
    loop {
        let step = machine.run(&mut world, slice).unwrap();
        stops.push((
            match &step {
                Step::Slice => "slice",
                Step::Parked(_) => "parked",
                Step::Ended(_) => "ended",
            }
            .into(),
            machine.meters().charged.unwrap(),
        ));
        if let Step::Ended(end) = step {
            return AccessObserved {
                end: Ok(crate::ExpectedEnd::from_end(end)),
                prints: world.prints,
                stops,
                memory: machine.meters().memory,
            };
        }
        assert!(matches!(step, Step::Slice), "the fixture has no effects");
        if cold {
            let sealed = machine.checkpoint().unwrap();
            machine = KernelMachine::open(
                program.clone(),
                bounds,
                &sealed.checkpoint(),
                Box::new(source.clone()),
            )
            .unwrap();
        }
    }
}

fn bounds() -> Bounds {
    Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 50_000_000,
        memory: 64 << 20,
        call_depth: 200,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
    }
}

fn without_plans(library: &lash_kernel_vm::PreparedLibrary) -> lash_kernel_vm::PreparedLibrary {
    let mut registry = library.registry().bodies();
    for (id, registered) in library.registry().iter() {
        if let Some(native) = registered.native()
            && registry.get(id).is_some_and(|f| f.native().is_none())
        {
            registry
                .implement(*id, std::sync::Arc::clone(native))
                .unwrap();
        }
    }
    // A native's decline body and every closed callee still need their
    // plans. Withdraw only the body-only access certificates, keeping
    // native template depth and the original plan dependency closure.
    let plans: Vec<_> = library
        .registry()
        .iter()
        .filter_map(|(_, registered)| {
            let plan = registered.plan()?;
            let access = plan
                .template
                .as_ref()
                .and_then(|template| template.heap.as_ref())
                .is_some_and(|heap| heap.element.is_some() || heap.record_write.is_some());
            if registered.native().is_some() || !access {
                return Some(plan);
            }
            Some(&*Box::leak(Box::new(lash_kernel_doc::plan::HelperPlan {
                name: plan.name,
                function: plan.function,
                callees: plan.callees,
                literals: plan.literals,
                fields: plan.fields,
                records: plan.records,
                registers: plan.registers,
                slots: plan.slots,
                closures: plan.closures,
                sites: plan.sites,
                run: plan.run,
                template: None,
            })))
        })
        .collect();
    registry.install_plans(&plans).unwrap();
    lash_kernel_vm::PreparedLibrary::with_helpers(
        std::sync::Arc::new(registry),
        std::sync::Arc::clone(library.helpers()),
    )
}

fn reference(program: &Program, bounds: Bounds, slice: u64, cold: bool) -> AccessObserved {
    let before = lash_kernel_vm::testing::resident_accesses();
    let result = observe(program, bounds, slice, cold);
    assert_eq!(
        lash_kernel_vm::testing::resident_accesses(),
        before,
        "a cached certificate must be withdrawn when plans are absent"
    );
    result
}

#[test]
fn dense_numeric_reads_and_plain_record_stores_preserve_source_boundaries() {
    let runner = runner::Runner::new("typescript").unwrap();
    let mut successful = 0;
    let mut source_library = None;
    for source in [
        "const xs = [1, 2, 3]; let i = 1; console.log(xs[i]); console.log(xs[-0]);",
        "const xs = [1, 2, 3]; let sum = 0; for (let i = 0; i < 4; i++) { sum += xs[0]; sum += xs[1]; sum += xs[2]; } console.log(sum);",
        "const r = {a: 1, b: 2, c: 3, d: 4}; for (let i = 0; i < 4; i++) { r.a = i; r.b = i; r.c = i; r.d = i; } console.log(r);",
        "const r = {a: 1}; const alias = r; r.a = 2; r.b = [3]; console.log(alias); console.log(Object.keys(r));",
        "const xs = [1, , undefined, 4]; for (const i of [0, 1, 2, 3, -1, 9, 1.5, NaN, Infinity]) { console.log(xs[i]); } console.log(xs['1']); console.log(xs.length);",
        "const r = Object.create(null); r.x = 2; r['0'] = 3; console.log(r.x); console.log(Object.keys(r));",
        "const key = {toString() { console.log('key'); return 'x'; }}; const r = {}; r[key] = 7; console.log(r.x);",
        "const r = {x: 1}; r.x = 2; r.__lash_brand = ['typescript', 'template']; try { r.x = 3; } catch (e) { console.log(e.name); } console.log(r);",
        "const a = [1]; const r = {}; r.a = a; r.b = r; a[0] = 7; console.log(r.a[0]);",
    ] {
        let case = runner::Case {
            id: "resident access".into(),
            dialect: "typescript".into(),
            source: source.into(),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        };
        let runner::Lowered::Program(program) = runner.lower(&case).unwrap() else {
            panic!("fixture must lower: {source}");
        };
        let general = Program {
            library: source_library
                .get_or_insert_with(|| without_plans(&program.library))
                .clone(),
            ..program.clone()
        };
        let hits = lash_kernel_vm::testing::resident_accesses();
        let result = observe(&program, bounds(), u64::MAX, false);
        successful += lash_kernel_vm::testing::resident_accesses() - hits;
        let expected = reference(&general, bounds(), u64::MAX, false);
        assert_eq!(result, expected, "{source}");
        let charged = result.stops.last().unwrap().1;
        for slice in [1, 2, 3, 7, 13, 31, 97, charged, charged + 1] {
            for cold in [false, true] {
                let result = observe(&program, bounds(), slice, cold);
                let expected = reference(&general, bounds(), slice, cold);
                assert_eq!(result, expected, "{source}; slice {slice}; cold {cold}");
            }
        }
        for limit in [0, 1, charged / 2, charged.saturating_sub(1), charged] {
            let bounds = Bounds {
                charge_limit: limit,
                ..bounds()
            };
            assert_eq!(
                observe(&program, bounds, 7, false),
                reference(&general, bounds, 7, false),
                "{source}; charge {limit}"
            );
        }
        for call_depth in 0..=5 {
            let bounds = Bounds {
                call_depth,
                ..bounds()
            };
            for slice in [13, u64::MAX] {
                assert_eq!(
                    observe(&program, bounds, slice, false),
                    reference(&general, bounds, slice, false),
                    "{source}; depth {call_depth}; slice {slice}"
                );
            }
        }
        // Include the operation's frame and a new member's growth in the
        // boundary sweep, rather than testing only an ample-memory run.
        for memory in std::iter::once(0)
            .chain((result.memory.saturating_sub(256)..=result.memory + 2048).step_by(32))
        {
            let bounds = Bounds { memory, ..bounds() };
            for slice in [13, u64::MAX] {
                assert_eq!(
                    observe(&program, bounds, slice, false),
                    reference(&general, bounds, slice, false),
                    "{source}; memory {memory}; slice {slice}"
                );
            }
        }
    }
    assert!(successful > 0, "the law must execute the resident path");
}

#[test]
fn resident_access_guards_keep_unproved_operands_on_the_helper() {
    let runner = runner::Runner::new("typescript").unwrap();
    let runner::Lowered::Program(base) = runner
        .lower(&runner::Case {
            id: "resident access guards".into(),
            dialect: "typescript".into(),
            source: "null;".into(),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        })
        .unwrap()
    else {
        panic!("the library fixture lowers");
    };
    let general = without_plans(&base.library);
    for (name, setup, args, expected_hits) in [
        ("ts.get_numeric", "", "[7], 0.0", 1),
        ("ts.get_numeric", "", "[(\"host\", 7)], -0.0", 1),
        ("ts.get_numeric", "", "[()], 0.0", 0),
        ("ts.get_numeric", "", "[absent], 0.0", 0),
        ("ts.get_numeric", "", "[7], -1.0", 0),
        ("ts.get_numeric", "", "[7], 2.0", 0),
        ("ts.get_numeric", "", "[7], 0.5", 0),
        ("ts.get_numeric", "", "[7], nan", 0),
        ("ts.get_numeric", "", "[7], inf", 0),
        ("ts.get_numeric", "", "(7, 8), 0.0", 0),
        ("ts.get_computed", "", "[7], \"0\"", 0),
        ("ts.get_computed", "", "[7], \"length\"", 0),
        ("ts.set", "let r = {x: 0}", "r, \"x\", [7]", 1),
        ("ts.set", "let r = {}", "r, \"x\", r", 1),
        ("ts.set", "let r = {__lash_brand: true}", "r, \"x\", 7", 0),
        (
            "ts.set",
            "let r = {} set r[\"__lash_brand\"] = true",
            "r, \"x\", 7",
            0,
        ),
        ("ts.set", "let r = {}", "r, 0.0, 7", 0),
        (
            "ts.set",
            "let r = {}",
            "r, \"a property longer than an inline key\", 7",
            0,
        ),
        ("ts.set", "let r = [0]", "r, 0.0, 7", 0),
    ] {
        let (id, _) = base
            .library
            .registry()
            .iter()
            .find(|(_, f)| f.definition().name.as_str() == name)
            .unwrap();
        let source = format!(
            "kernel 1\nnumbers by_spelling\nmain {{ {setup} let answer = invoke @{id}({args}) return answer }}"
        );
        let mut document = lash_kernel_doc::parse_document(&source).unwrap();
        document.library.insert(*id);
        let program = crate::linked::program(std::sync::Arc::new(document), base.library.clone());
        let reference_program = Program {
            library: general.clone(),
            ..program.clone()
        };
        let before = lash_kernel_vm::testing::resident_accesses();
        let result = observe(&program, bounds(), u64::MAX, false);
        assert_eq!(
            lash_kernel_vm::testing::resident_accesses() - before,
            expected_hits,
            "{name}({args})"
        );
        assert_eq!(
            result,
            reference(&reference_program, bounds(), u64::MAX, false),
            "{name}({args})"
        );
        for slice in [1, 3, 13] {
            assert_eq!(
                observe(&program, bounds(), slice, true),
                reference(&reference_program, bounds(), slice, true),
                "{name}({args}); slice {slice}"
            );
        }
    }
}
