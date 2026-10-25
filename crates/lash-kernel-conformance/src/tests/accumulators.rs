//! FIG-6795: a linear text accumulator's concatenation takes the storage of
//! the text its variable held, in the register program. A concatenation
//! that took the storage of a text an older version still observes, or
//! gave it back wrong at an exit, makes a run read a moved text: its end,
//! charge or held bytes then differ from the run's under another layout,
//! slice or memory meter, or from the value the program states.

use lash_kernel_doc::{Datum, print_document};
use lash_kernel_vm::kernel_internals::{Layout, in_layout};
use lash_kernel_vm::{Machine, MemoryMeter, PreparedLibrary, Program};

use crate::machine::case_program;
use crate::projection::Plan;
use crate::{Case, Environment, Expected, ExpectedEnd};

/// The kernel's own accumulators, each with the value it finishes with.
fn kernel_programs() -> Vec<(&'static str, &'static str, Datum)> {
    let text = |text: String| Datum::Text(text);
    let mut built = String::from("a text past the inline limit");
    for i in 0..12 {
        built.push_str(&format!("-row-{i}"));
    }
    let start = String::from("a prefix past the inline limit");
    let mut s = start.clone();
    for i in 0..9 {
        s.push_str(&i.to_string());
    }
    let mut kept = Vec::new();
    for j in 0..9 {
        kept.push(text(s.clone()));
        s.push_str(&j.to_string());
    }
    vec![
        (
            "a text accumulated by concatenation, no older version of it observed",
            "definition \"function build(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let i = 0 let s = text.concat(\\\"a text past the inline\\\", \\\" limit\\\") while num.lt(i, n) { set s = text.concat(s, text.concat(\\\"-row-\\\", num.to_text(i))) set i = num.add(i, 1) } return s }\"\n\nmain { let r = call build(12) finish r }",
            text(built),
        ),
        (
            "a text accumulated over a prefix another variable holds, then with its older versions kept",
            "definition \"function build(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let i = 0 let start = text.concat(\\\"a prefix past the inline\\\", \\\" limit\\\") let s = start while num.lt(i, n) { set s = text.concat(s, num.to_text(i)) set i = num.add(i, 1) } let kept = [] let prev = s let j = 0 while num.lt(j, n) { set prev = s set s = text.concat(s, num.to_text(j)) set kept[list.len(kept)] = prev set j = num.add(j, 1) } return (start, s, kept) }\"\n\nmain { let r = call build(9) finish r }",
            Datum::Tuple(vec![text(start), text(s), Datum::List(kept)]),
        ),
    ]
}

fn kernel_case(name: &str, text: &str) -> Case {
    let registry = super::corpus_registry();
    let source = format!("kernel 1\nnumbers by_spelling\n{text}");
    let document = crate::document_fixture::authored(&source, &registry);
    let mut environment = Environment::default();
    environment.bounds.call_depth = 16;
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

/// The end, charge and final held bytes of `case` under `program` at
/// `slice`, its census counting.
fn finished<const L: u64>(
    program: &Program,
    case: &Case,
    slice: u64,
) -> (ExpectedEnd, Option<u64>, u64) {
    let start = lash_kernel_vm::CellStart {
        target: lash_kernel_vm::Target::Main,
        args: Vec::new(),
        session: None,
        owner: crate::pages::OWNER,
        modes: Default::default(),
        host: Vec::new(),
    };
    let mut machine = Plan::<L>::start(
        program.clone(),
        case.environment.bounds.into(),
        start,
        Box::new(crate::pages::Source::default()),
    )
    .unwrap_or_else(|error| panic!("{}: {error}", case.name));
    let mut world = crate::helper_plans::World::default();
    loop {
        match machine.run(&mut world, slice) {
            Ok(lash_kernel_vm::Step::Slice) => {}
            Ok(lash_kernel_vm::Step::Ended(end)) => {
                let meters = machine.meters();
                return (ExpectedEnd::from_end(end), meters.charged, meters.memory);
            }
            Ok(_) => panic!("{}: a run without effects parked", case.name),
            Err(error) => panic!("{}: {error}", case.name),
        }
    }
}

/// The memory bound under which a run's held bytes refuse it.
const TIGHT: u64 = 96 * 1024;

/// Every accumulator, in the kernel's own programs and in both dialects',
/// extends its text in place, except the aliased-prefix control, whose
/// every older version stays observed. Each makes the run its program states
/// under every slice, memory meter and bound and layout, the natural one
/// beside one whose natives with kernel bodies run those bodies: the same
/// end, charge and held bytes, and for the kernel's programs the value they
/// state.
#[test]
fn linear_text_accumulators_keep_every_version_they_extend() {
    let registry = std::sync::Arc::new(super::corpus_registry());
    let kernel = lash_kernel_vm::kernel_internals::prepare_with_layout(registry, Layout(0));
    let mut cases: Vec<(Case, PreparedLibrary, Option<Datum>, bool)> = kernel_programs()
        .into_iter()
        .map(|(name, text, value)| (kernel_case(name, text), kernel.clone(), Some(value), true))
        .collect();
    for (lowered, program) in crate::helper_plans::text_accumulator_programs() {
        let extends = !lowered
            .id
            .ends_with(crate::helper_plans::ALIASED_ACCUMULATOR);
        let mut case = kernel_case(&lowered.id, "main { return null }");
        case.document = print_document(&program.document);
        cases.push((case, program.library.clone(), None, extends));
    }
    let mut failures = Vec::new();
    for (case, library, value, extends) in &cases {
        let natural = case_program(&in_layout(library, Layout(0)), case)
            .unwrap_or_else(|| panic!("{} is admitted", case.name));
        let bodies = Program {
            library: in_layout(library, Layout(3)),
            ..natural.clone()
        };
        let (reference, census) =
            crate::projection::counted(|| finished::<0>(&natural, case, u64::MAX));
        if (census.accumulated > 0) != *extends {
            failures.push(format!(
                "{}: extended its text {} times: {census:?}",
                case.name, census.accumulated
            ));
        }
        if let Some(value) = value
            && reference.0 != ExpectedEnd::Finished(value.clone())
        {
            failures.push(format!("{}: finished {:?}", case.name, reference.0));
        }
        for (memory_meter, memory) in [
            (MemoryMeter::On, None),
            (MemoryMeter::Off, None),
            (MemoryMeter::On, Some(TIGHT)),
        ] {
            let mut metered = case.clone();
            metered.environment.bounds.memory_meter = memory_meter;
            if let Some(memory) = memory {
                metered.environment.bounds.memory = memory;
            }
            let unsliced = finished::<0>(&natural, &metered, u64::MAX);
            if memory.is_none() && (&unsliced.0, unsliced.1) != (&reference.0, reference.1) {
                failures.push(format!(
                    "{}, memory {memory_meter:?}: {unsliced:?}",
                    case.name
                ));
            }
            for slice in [7, 50, u64::MAX] {
                for run in [
                    finished::<0>(&natural, &metered, slice),
                    finished::<3>(&bodies, &metered, slice),
                ] {
                    if run != unsliced {
                        failures.push(format!(
                            "{}, memory {memory_meter:?} {memory:?}, slice {slice}: {run:?} where unsliced {unsliced:?}",
                            case.name
                        ));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
