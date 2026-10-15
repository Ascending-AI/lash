//! Numeric helper rewrites preserve ECMA values and K-MACH-008 resume,
//! while K-CHG-007 charges the smaller bodies for the work they now do.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_dialect::NamedLibrary;
use lash_kernel_doc::{Datum, ErrorDatum, Float, FunctionId, Handle, Integer, Name, Timestamp};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, Machine, PreparedLibrary, Program, Step, Target,
};

// The previous definitions are an independent oracle and exact old pins.
const BEFORE: &str = r#"function ts.add(left: Any, right: Any) -> Any
kernel 1
errors "type_error"
charge sum(12, deep(left), deep(right))
body {
  # Two numbers add and two texts join, whatever their conversions would
  # say.
  let left_kind = kind(left)
  if same(left_kind, kind(right)) {
    if same(left_kind, "float") { return num.add(left, right) }
    if same(left_kind, "text") { return text.concat(left, right) }
  }
  let a = invoke ts.to_primitive(left, "default")
  let b = invoke ts.to_primitive(right, "default")
  let textual = same(kind(a), "text")
  if textual {} else {
    set textual = same(kind(b), "text")
  }
  if textual {
    let sa = invoke ts.primitive_to_string(a)
    let sb = invoke ts.primitive_to_string(b)
    return text.concat(sa, sb)
  }
  let na = invoke ts.primitive_to_number(a)
  let nb = invoke ts.primitive_to_number(b)
  return num.add(na, nb)
}

function ts.sub(left: Any, right: Any) -> Number
kernel 1
errors "type_error"
charge sum(12, deep(left), deep(right))
body {
  let a = invoke ts.to_number(left)
  let b = invoke ts.to_number(right)
  return num.sub(a, b)
}

function ts.lt(left: Any, right: Any) -> Bool
kernel 1
errors "type_error"
charge sum(20, deep(left), deep(right))
body {
  let operands = invoke ts.relational(left, right)
  if operands[0] {
    return num.lt(text.utf16_compare(operands[1], operands[2]), 0)
  }
  return num.lt(operands[1], operands[2])
}
"#;

const BOUNDS: Bounds = Bounds {
    charge: 10_000_000,
    memory: 64 << 20,
    call_depth: 200,
    live_tasks: 10,
    requests_per_park: 10,
    join_members: 10,
};

struct Kernels {
    old: BTreeMap<String, FunctionId>,
    new: BTreeMap<String, FunctionId>,
    prepared: PreparedLibrary,
    registry: Arc<lash_kernel_doc::FunctionRegistry>,
}

fn kernels() -> Kernels {
    let mut registry = super::kernel_registry();
    let mut library = NamedLibrary::from_registry(&registry).unwrap();
    for definition in crate::define_helpers(&mut library).unwrap() {
        registry.register(definition, None).unwrap();
    }
    let new = registry
        .iter()
        .map(|(id, held)| (held.definition().name.to_string(), *id))
        .collect();
    let mut library = NamedLibrary::resolving(&registry, |id| {
        !matches!(
            registry.get(id).unwrap().definition().name.as_str(),
            "ts.add" | "ts.sub" | "ts.lt"
        )
    })
    .unwrap();
    let source = format!(
        "use kind\nuse same\nuse num.add\nuse num.sub\nuse num.lt\nuse text.concat\nuse text.utf16_compare\nuse ts.to_primitive\nuse ts.primitive_to_string\nuse ts.primitive_to_number\nuse ts.to_number\nuse ts.relational\n{BEFORE}"
    );
    let mut old = BTreeMap::new();
    // Retained definitions keep their exact identities and transitive pins;
    // old documents run on the successor registry, not a rewritten document.
    for definition in lash_kernel_dialect::define_functions(&source, &mut library).unwrap() {
        let id = definition.identity().unwrap();
        old.insert(definition.name.to_string(), id);
        if registry.get(&id).is_none() {
            registry.register(definition, None).unwrap();
        }
    }
    let registry = Arc::new(registry);
    Kernels {
        old,
        new,
        prepared: PreparedLibrary::new(registry.clone()),
        registry,
    }
}

fn program(kernels: &Kernels, id: FunctionId) -> Program {
    let mut document = lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers float\nuse operation = @{id}\nentry probe(left: Any, right: Any) -> Any\nfn probe(left, right) {{ let out = invoke operation(left, right) return out }}\nmain {{ }}\n"
    )).unwrap();
    document.manifest.functions =
        lash_kernel_check::requirements(&document, kernels.registry.as_ref()).functions;
    Program {
        release: super::pages::RELEASE,
        document: Arc::new(document),
        library: kernels.prepared.clone(),
    }
}

#[derive(Default)]
struct Console;
impl Host for Console {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        panic!("the law reads no projection")
    }
    fn print(&mut self, _: &Datum) {
        panic!("the scalar law prints nothing")
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

#[derive(Debug, PartialEq)]
struct Ran {
    end: String,
    charged: u64,
}

fn run(program: &Program, left: &Datum, right: &Datum, relay: bool) -> Ran {
    let source = super::pages::Source::default();
    let mut machine = super::pages::with_source(
        program.clone(),
        BOUNDS,
        CellStart {
            target: Target::Entry(Name::new("probe")),
            args: vec![left.clone(), right.clone()],
            session: None,
            owner: super::pages::OWNER,
            modes: Default::default(),
            host: Vec::new(),
        },
        lash_kernel_vm::Layout::default(),
        &source,
    )
    .unwrap();
    loop {
        match machine
            .run(&mut Console, if relay { 1 } else { u64::MAX })
            .unwrap()
        {
            Step::Slice => {
                super::pages::reopen(
                    &mut machine,
                    program,
                    BOUNDS,
                    &source,
                    lash_kernel_vm::Layout::default(),
                );
            }
            Step::Ended(end) => {
                assert!(
                    matches!(
                        end,
                        End::Finished(_) | End::Error(lash_kernel_vm::RunError::Uncaught(_))
                    ),
                    "{end:?}"
                );
                // Debug preserves the float's signed zero and NaN spelling.
                return Ran {
                    end: format!("{end:?}"),
                    charged: machine.meters().charged,
                };
            }
            Step::Parked(park) => panic!("scalar arithmetic parked: {park:?}"),
        }
    }
}

fn float(value: f64) -> Datum {
    Datum::Float(Float::new(value))
}

/// ECMA numeric operators, K-CHG-007 and K-MACH-008: old exact pins stay
/// runnable, every new numeric/fallback path survives export/import, and
/// primitive numeric work is charged less without taxing float-pair add.
#[test]
fn numeric_helpers_preserve_values_and_resume_with_less_work() {
    let kernels = kernels();
    let values = [
        float(0.0), float(-0.0), float(f64::NAN), float(f64::INFINITY),
        float(f64::NEG_INFINITY), float(1.5), float(-2.0), float(f64::MIN_POSITIVE),
        Datum::Int(Integer::from(1)), Datum::Int(Integer::from(i64::MAX)),
        Datum::Int(Integer::from(i64::MIN)),
        Datum::Int(Integer::from(9_007_199_254_740_993_i64)),
        Datum::Int(Integer::parse("10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000").unwrap()),
        Datum::Text("2".into()), Datum::Text("x".into()), Datum::Bool(true), Datum::Null, Datum::Absent,
        float(f64::from_bits(1)), float(f64::MAX),
    ];
    let mut comparisons = 0;
    for name in ["ts.add", "ts.sub", "ts.lt"] {
        let old = program(&kernels, kernels.old[name]);
        let new = program(&kernels, kernels.new[name]);
        for left in &values {
            for right in &values {
                let before = run(&old, left, right, false);
                let after = run(&new, left, right, false);
                assert_eq!(before.end, after.end, "{name}({left:?}, {right:?})");
                assert_eq!(run(&new, left, right, true), after, "{name} sliced");
                comparisons += 1;
            }
        }
        let a = float(2.0);
        let b = float(-0.0);
        let before = run(&old, &a, &b, false).charged;
        let after = run(&new, &a, &b, false).charged;
        if name == "ts.add" {
            assert_eq!(after, before, "float-pair add keeps its first exit");
            for (left, right) in [(&a, &values[8]), (&values[8], &a)] {
                assert!(
                    run(&new, left, right, false).charged < run(&old, left, right, false).charged,
                    "mixed addition skips coercion helpers"
                );
            }
        } else {
            assert!(
                after < before,
                "{name} skips float coercion helpers: {after} >= {before}"
            );
        }
        // Old pins also resume on the registry that holds new definitions.
        assert_eq!(run(&old, &a, &b, true), run(&old, &a, &b, false));
    }
    println!("numeric helper value/resume comparisons={comparisons}");
}
