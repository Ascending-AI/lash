//! Accumulation agrees with the semantic operators, including refusing guards.

use super::*;
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::plan as p;
use lash_kernel_value::FunctionId;
use std::collections::BTreeMap;

fn probes(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut probes = call_probes(registry, ids);
    for (definition, native) in lash_kernel_lib::numbers()
        .into_iter()
        .chain(lash_kernel_lib::text_json())
    {
        let name = match definition.name.as_str() {
            "num.to_text" => "num.to_text",
            "text.join" => "text.join",
            "format.fixed" => "format.fixed",
            "format.radix" => "format.radix",
            _ => continue,
        };
        probes.push((name, registry.register(definition, Some(native)).unwrap()));
    }
    let concat = ids["text.concat"];
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function law.append(a: Any, b: Any) -> Any\nkernel 1\ncharge sum(7, deep(a), size(b), deep(result))\nnative\nuse text.concat = @{concat}\nbody {{ return text.concat(a, b) }}"
    )).unwrap();
    let function = registry.register(definition, None).unwrap();
    let plan = Box::leak(Box::new(p::HelperPlan {
        name: "law.append",
        function,
        callees: Box::leak(Box::new([concat])),
        literals: &[],
        fields: &[],
        records: &[],
        registers: 2,
        slots: &[],
        closures: &[],
        sites: &[],
        run: |_, _| unreachable!("a library body is the fallback"),
        template: Some(p::HelperTemplate {
            entry: None,
            metered: None,
            depth: 1,
            callees: Box::leak(Box::new([concat])),
            heap: Some(p::HeapTemplate {
                entry: Some(|heap, args, _| heap.call(0, args)),
                depth: 1,
                writes: false,
                copy: None,
                element: None,
                record_write: None,
                callees: Box::leak(Box::new([concat])),
            }),
            forwards: p::Forwards {
                texts: Some(0),
                ..Default::default()
            },
        }),
    }));
    registry.install_plans(&[plan]).unwrap();
    probes.push(("law.append", function));
    probes
}

fn native_probes(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut functions = probes(registry, ids);
    for (mut definition, native) in lash_kernel_lib::text_json() {
        if definition.name.as_str() != "text.concat" {
            continue;
        }
        definition.name = lash_kernel_value::FunctionName::new("law.native_concat").unwrap();
        functions.push((
            "law.native_concat",
            registry.register(definition, Some(native)).unwrap(),
        ));
    }
    functions
}

type Observation = (
    String,
    Vec<Datum>,
    Vec<(&'static str, crate::Meters, [u64; 3])>,
    Vec<lash_kernel_state::semantic::Checkpoint>,
    Vec<Request>,
    Vec<lash_kernel_exec::Weight>,
);

fn run(source: &str, record: bool, slice: u64, bounds: Bounds, layout: Layout) -> Observation {
    run_selected(
        source,
        record,
        slice,
        bounds,
        layout,
        (super::ExecutionMode::Specialized, false),
        false,
    )
}

fn run_selected(
    source: &str,
    record: bool,
    slice: u64,
    bounds: Bounds,
    layout: Layout,
    (mode, cold): (super::ExecutionMode, bool),
    native: bool,
) -> Observation {
    let mut embedder = Embedder::with(
        source,
        Setup {
            probes: Some(if native { native_probes } else { probes }),
            bounds,
            slice,
            layout,
            ..Setup::default()
        },
    );
    let tape = crate::ChargeTape::new();
    if record {
        embedder.machine.record_charges(tape.clone());
    }
    let (mut stops, mut checkpoints) = (Vec::new(), Vec::new());
    let end = loop {
        match mode {
            super::ExecutionMode::Source => embedder.machine.force_source_execution(),
            super::ExecutionMode::Generic => embedder.machine.force_generic_packets(),
            super::ExecutionMode::Specialized => {}
        }
        let step = embedder.run();
        let kind = match &step {
            Step::Ended(_) => "ended",
            Step::Slice => "slice",
            Step::Parked(_) => "parked",
        };
        let balance = embedder
            .machine
            .logical_balance()
            .expect("source owner balance");
        stops.push((
            kind,
            embedder.machine.meters(),
            [
                balance.entity_local_bytes,
                balance.root_and_control_bytes,
                balance.entities,
            ],
        ));
        if cold && !matches!(step, Step::Ended(_)) {
            checkpoints.push(embedder.machine.semantic_checkpoint());
            embedder.reopen(layout, true);
            if record {
                embedder.machine.record_charges(tape.clone());
            }
        }
        match step {
            Step::Ended(End::Finished(finished)) => {
                break format!("finished {:?}", finished.result);
            }
            Step::Ended(other) => break format!("{other:?}"),
            Step::Slice => {}
            Step::Parked(_) => {
                if !cold {
                    checkpoints.push(embedder.machine.semantic_checkpoint());
                }
                embedder.deliver_next();
            }
        }
    };
    (
        end,
        embedder.world.printed,
        stops,
        checkpoints,
        embedder.requests,
        lash_kernel_exec::Weight::expand(tape.steps()),
    )
}

/// K-CHG-001 / K-CHG-003 / K-MACH-008: accumulation retains every value, stop,
/// charge and held byte when its scalar or text guard answers or fails.
#[test]
fn accumulations_agree() {
    prepared_text_prefixes();
    use crate::machine::islands::witness::{self, Exit, Family};
    witness::begin();
    let source = scalar_document(
        r#"let s = "" let sum = 0 let i = 0 while flat.lt(i, 40) { set s = law.append(s, "é😀") set sum = flat.add(sum, 3) set i = flat.add(i, 1) } do perform 0("middle") as Any let old = s let j = 0 while flat.lt(j, 3) { set s = text.concat(s, "!") set j = flat.add(j, 1) } let huge = 9223372036854775807 try { set huge = flat.add(huge, 1) } catch err { print huge } let f = -0.0 set f = flat.add(f, 0.0) return (s, old, sum, huge, f)"#,
    );
    let without_tape = |(a, b, c, d, e, _): Observation| (a, b, c, d, e);
    let taken = || crate::machine::islands::ACCUMULATED.with(|n| n.replace(0));
    let reference_body = run(
        &source,
        true,
        u64::MAX,
        super::super::embedder::ROOMY,
        Layout(1),
    );
    let run = |record, slice, bounds| run(&source, record, slice, bounds, Layout(0));
    taken();
    let reference = run(true, u64::MAX, super::super::embedder::ROOMY);
    crate::machine::islands::TEXT_APPENDED.with(|n| n.set(0));
    let natural = run(false, u64::MAX, super::super::embedder::ROOMY);
    assert_eq!(
        without_tape(natural.clone()),
        without_tape(reference.clone())
    );
    assert_eq!(without_tape(natural), without_tape(reference_body));
    assert!(
        taken() > 0,
        "accumulation must run as one register instruction"
    );
    assert!(
        crate::machine::islands::TEXT_APPENDED.with(|n| n.get()) > 0,
        "the helper append runs directly"
    );
    for slice in [1, 2, 3, 5, 11, 31, 67] {
        assert_eq!(
            without_tape(run(false, slice, super::super::embedder::ROOMY)),
            without_tape(run(true, slice, super::super::embedder::ROOMY)),
            "slice {slice}"
        );
    }
    let charge = reference.2.last().unwrap().1.charged.unwrap();
    for limit in (0..charge).step_by(97) {
        let bounds = Bounds {
            charge_limit: limit,
            ..super::super::embedder::ROOMY
        };
        assert_eq!(
            without_tape(run(false, u64::MAX, bounds)),
            without_tape(run(true, u64::MAX, bounds)),
            "charge {limit}"
        );
    }
    for memory in [4096, 16384, 65536, 131072] {
        let bounds = Bounds {
            memory,
            ..super::super::embedder::ROOMY
        };
        assert_eq!(
            without_tape(run(false, u64::MAX, bounds)),
            without_tape(run(true, u64::MAX, bounds)),
            "memory {memory}"
        );
    }
    for slice in [u64::MAX, 1, 7, 31] {
        let cold = slice != u64::MAX;
        let expected = run_selected(
            &source,
            false,
            slice,
            super::super::embedder::ROOMY,
            Layout(0),
            (super::ExecutionMode::Source, cold),
            false,
        );
        for mode in [
            super::ExecutionMode::Generic,
            super::ExecutionMode::Specialized,
        ] {
            let actual = run_selected(
                &source,
                false,
                slice,
                super::super::embedder::ROOMY,
                Layout(0),
                (mode, cold),
                false,
            );
            assert_eq!(
                actual.0, expected.0,
                "owned append result {mode:?}, slice={slice}"
            );
            assert_eq!(
                actual.1, expected.1,
                "owned append prints {mode:?}, slice={slice}"
            );
            assert_eq!(
                actual.4, expected.4,
                "owned append requests {mode:?}, slice={slice}"
            );
            assert_eq!(actual.5, expected.5, "owned append canonical charge tape");
            assert_eq!(actual.2.len(), expected.2.len(), "owned append stop count");
            for (at, (actual, expected)) in actual.2.iter().zip(&expected.2).enumerate() {
                assert_eq!(
                    actual, expected,
                    "owned append stop {at}, {mode:?}, slice={slice}"
                );
            }
            assert_eq!(
                actual.3.len(),
                expected.3.len(),
                "owned append cold boundary count"
            );
            for (at, (actual, expected)) in actual.3.iter().zip(&expected.3).enumerate() {
                assert!(
                    actual == expected,
                    "owned append cold state {at}, {mode:?}, slice={slice}"
                );
            }
            assert_eq!(
                actual, expected,
                "owned append {mode:?}, cold={cold}, slice={slice}"
            );
        }
    }
    let formatted = scalar_document(
        r#"let s = "abcdefghijklmnopé😀" let old = s let i = 0 while flat.lt(i, 3) { set s = text.concat(s, text.join(["é😀", num.to_text(-0.0), format.fixed(1.25, 2), format.radix(255, 16)], "|")) set i = flat.add(i, 1) } do perform 0(s) as Any return (s, old)"#,
    );
    crate::machine::islands::take_text_publications();
    for slice in [u64::MAX, 1, 7] {
        let cold = slice != u64::MAX;
        let expected = run_selected(
            &formatted,
            false,
            slice,
            super::super::embedder::ROOMY,
            Layout(0),
            (super::ExecutionMode::Source, cold),
            false,
        );
        for mode in [
            super::ExecutionMode::Generic,
            super::ExecutionMode::Specialized,
        ] {
            assert_eq!(
                run_selected(
                    &formatted,
                    false,
                    slice,
                    super::super::embedder::ROOMY,
                    Layout(0),
                    (mode, cold),
                    false,
                ),
                expected,
                "canonical formatted pieces {mode:?}, cold={cold}, slice={slice}"
            );
        }
    }
    let counts = witness::take();
    for exit in [Exit::Commit, Exit::Guard, Exit::Prefix] {
        assert!(
            counts
                .get(&(Family::TextAppend, exit))
                .copied()
                .unwrap_or(0)
                > 0,
            "owned appends must reach {exit:?}: {counts:?}"
        );
    }
    println!("owned append exits: {counts:?}");
    let native_formatted = scalar_document(
        r#"let s = "abcdefghijklmnopé😀" let old = s let parts = ["é😀", "tag"] let separator = "|" let i = 0 while flat.lt(i, 3) { let joined = text.join(parts, separator) set s = law.native_concat(s, joined) set i = flat.add(i, 1) } do perform 0(s) as Any return (s, old)"#,
    );
    crate::machine::islands::take_text_publications();
    let mut native_publications = [0; 2];
    for record in [false, true] {
        for slice in [u64::MAX, 1, 7] {
            let cold = slice != u64::MAX;
            let expected = run_selected(
                &native_formatted,
                record,
                slice,
                super::super::embedder::ROOMY,
                Layout(0),
                (super::ExecutionMode::Source, cold),
                true,
            );
            for mode in [
                super::ExecutionMode::Generic,
                super::ExecutionMode::Specialized,
            ] {
                crate::machine::islands::take_text_publications();
                let actual = run_selected(
                    &native_formatted,
                    record,
                    slice,
                    super::super::embedder::ROOMY,
                    Layout(0),
                    (mode, cold),
                    true,
                );
                let publications = crate::machine::islands::take_text_publications();
                if slice == u64::MAX {
                    assert!(
                        publications.iter().all(|count| *count > 0),
                        "native text must use staged source acceptance with record={record}, mode={mode:?}: {publications:?}"
                    );
                }
                for (sum, count) in native_publications.iter_mut().zip(publications) {
                    *sum += count;
                }
                assert_eq!(
                    actual, expected,
                    "native text source/packet/cold agreement {mode:?}, slice={slice}"
                );
            }
        }
    }
    let text_certificates = native_publications;
    assert!(
        text_certificates.iter().all(|count| *count > 0),
        "native concat and join must publish through staged text storage: {text_certificates:?}"
    );
}

/// K-MACH-008 / LogicalHeldV1: a text prefix publishes its actual live-outs
/// once, retains temporary scratch peaks, and restores every donated buffer on
/// rejection. This directly executes the staged consumer even before core
/// enables authenticated text activation in production.
fn prepared_text_prefixes() {
    use crate::machine::islands::cx::Cx;
    use lash_kernel_doc::Value;
    use lash_kernel_doc::arena::{Body, Chunk, ShortText};
    use lash_kernel_doc::heap::{HeapFault, TextSummary};
    use lash_kernel_state::arena::Arena;
    use lash_kernel_state::kernel_internals::{
        PagedHeap,
        logical::{Ledger, Owner, RootValue},
    };
    use lash_kernel_state::paged::{EditEpoch, SemanticHeap};

    let source = super::super::embedder::Source::default();
    let heap = PagedHeap::fresh(Box::new(source.clone()), EditEpoch(1));
    let mut ledger = Ledger::fresh();
    let mut arena = Arena::new();
    let original = "abcdefghijklmnopé😀";
    let left = arena.text(original);
    let left_handle = left.handle().unwrap();
    let right = Value::Text(ShortText::new("é😀").unwrap());
    let owner = Owner::Session([11; 32]);
    ledger
        .prepare_source_boundary(
            arena.registration().unwrap(),
            &heap,
            &[],
            &[(owner, RootValue::Value(&left))],
        )
        .unwrap()
        .admit(u64::MAX)
        .unwrap_or_else(|_| panic!("entry fits"))
        .publish(&mut ledger, |_, edits| assert!(edits.is_empty()))
        .unwrap();
    let before = ledger.balance().unwrap();
    let held = arena.held();
    let version = arena.content_version(left_handle).unwrap();
    let read = |arena: &Arena, value: Value| match value {
        Value::Text(text) => text.as_str().to_owned(),
        Value::LongText(handle) => match arena.body(handle).unwrap() {
            Body::LongText(text) => match &*text.chunks {
                [Chunk::Resident(text)] => text.clone(),
                other => panic!("expected resident text, got {other:?}"),
            },
            _ => panic!("text body"),
        },
        _ => panic!("text value"),
    };

    // Decline after a chain of physical moves. A saved operand cannot name an
    // interior predecessor whose storage has moved, even though its fee facts
    // and original summary still exist.
    for surviving in [0, 2, 3] {
        let result = Cx::prepare_text_prefix(
            &mut arena,
            &ledger,
            &heap,
            &[left, right],
            &[left_handle],
            None,
            &[(0, 1, true), (2, 1, true)],
            &[(owner, surviving)],
            &[owner],
        )
        .unwrap()
        .unwrap()
        .boundary(&mut ledger, &heap, |transaction, _, _, facts| {
            assert_eq!(facts.len(), 2);
            assert_eq!(
                facts[0].summaries[0],
                TextSummary {
                    utf8_bytes: 22,
                    chars: 18,
                    utf16_units: 19
                }
            );
            assert_eq!(
                facts[0].summaries[1],
                TextSummary {
                    utf8_bytes: 6,
                    chars: 2,
                    utf16_units: 3
                }
            );
            assert_eq!(
                facts[1].summaries[2],
                TextSummary {
                    utf8_bytes: 34,
                    chars: 22,
                    utf16_units: 25
                }
            );
            assert!(transaction.admit(0).is_err());
            Ok(())
        })
        .unwrap();
        assert!(result.is_none());
        assert_eq!(read(&arena, left), original);
        assert!(arena.content_is(&version));
        assert_eq!(arena.held(), held);
        assert_eq!(ledger.balance().unwrap(), before);
    }

    // The canonical immediate concatenation allocates neither an output body
    // nor native scratch. A positive scratch reservation would refuse valid
    // short concatenations at their exact memory bound.
    let short = Value::Text(ShortText::new("a").unwrap());
    assert!(
        Cx::prepare_text_prefix(
            &mut arena,
            &ledger,
            &heap,
            &[short, right],
            &[],
            None,
            &[(0, 1, false)],
            &[(owner, 2)],
            &[owner],
        )
        .unwrap()
        .unwrap()
        .boundary(&mut ledger, &heap, |_, _, _, facts| {
            assert_eq!(facts[0].work, 0);
            assert_eq!(facts[0].install, 0);
            Ok(())
        },)
        .unwrap()
        .is_none()
    );

    // A large temporary join followed by a smaller live-out cannot hide its
    // scratch peak. These are prepared pieces, not a second formatter.
    let large = arena.text(&"é😀".repeat(32));
    let parts = arena.create_list(&[large, right]);
    let empty = Value::Text(ShortText::new("").unwrap());
    let held = arena.held();
    let result = Cx::prepare_text_prefix(
        &mut arena,
        &ledger,
        &heap,
        &[left, parts, empty, right],
        &[left_handle],
        Some((1, 2)),
        &[(0, 3, true)],
        &[(owner, 5)],
        &[owner],
    )
    .unwrap()
    .unwrap()
    .boundary(&mut ledger, &heap, |transaction, _, _, facts| {
        assert!(facts[0].arguments[0].same(&parts));
        assert!(facts[0].arguments[1].same(&empty));
        assert_eq!(facts[0].summaries[2].utf8_bytes, 198);
        assert_eq!(facts[0].work, 198);
        assert!(facts[0].install > 0);
        assert_eq!(facts[1].summaries[2].utf8_bytes, 28);
        let required = transaction.require_source_room(facts[0].work).unwrap();
        assert!(required.admit(before.total().unwrap() + 197).is_err());
        Ok(())
    })
    .unwrap();
    assert!(result.is_none());
    assert_eq!(read(&arena, left), original);
    assert_eq!(arena.held(), held);
    assert_eq!(ledger.balance().unwrap(), before);

    // Errors and stale entry dependencies must both unwind storage. Returning
    // Ok without invoking the accepted publication also consumes no storage.
    let failed = Cx::prepare_text_prefix(
        &mut arena,
        &ledger,
        &heap,
        &[left, right],
        &[left_handle],
        None,
        &[(0, 1, true)],
        &[(owner, 2)],
        &[owner],
    )
    .unwrap()
    .unwrap()
    .boundary(&mut ledger, &heap, |_, _, _, _| {
        Err::<(), _>(HeapFault::Malformed("declined source exit".into()))
    });
    assert!(failed.is_err());
    assert_eq!(read(&arena, left), original);
    let result = Cx::prepare_text_prefix(
        &mut arena,
        &ledger,
        &heap,
        &[left, right],
        &[left_handle],
        None,
        &[(0, 1, true)],
        &[(owner, 2)],
        &[owner],
    )
    .unwrap()
    .unwrap()
    .boundary(&mut ledger, &heap, |transaction, ledger, apply, _| {
        ledger.handoff()?;
        assert!(
            transaction
                .admit(u64::MAX)
                .unwrap_or_else(|_| panic!("roomy"))
                .publish(ledger, apply)
                .is_err()
        );
        Ok(())
    })
    .unwrap();
    assert!(result.is_none());
    assert_eq!(read(&arena, left), original);
    assert_eq!(arena.held(), held);

    // Shared/self operands copy; moving them would corrupt the old alias.
    let result = Cx::prepare_text_prefix(
        &mut arena,
        &ledger,
        &heap,
        &[left],
        &[],
        None,
        &[(0, 0, true)],
        &[(owner, 1)],
        &[owner],
    )
    .unwrap()
    .unwrap()
    .boundary(&mut ledger, &heap, |transaction, ledger, apply, facts| {
        assert!(facts[0].arguments.iter().all(|value| value.same(&left)));
        assert_eq!(facts[0].work, 44);
        transaction
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("roomy"))
            .publish(ledger, apply)
    })
    .unwrap()
    .expect("prepared consumer published");
    assert_eq!(read(&arena, left), original);
    assert_eq!(read(&arena, result.1[0]), format!("{original}{original}"));
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 16 + 44);

    // Donation across multiple appends commits exactly one final root. No
    // per-append journal or reconstructed old buffer participates in success.
    let current = result.1[0];
    let result = Cx::prepare_text_prefix(
        &mut arena,
        &ledger,
        &heap,
        &[current, right],
        &[current.handle().unwrap()],
        None,
        &[(0, 1, true), (2, 1, true)],
        &[(owner, 3)],
        &[owner],
    )
    .unwrap()
    .unwrap()
    .boundary(&mut ledger, &heap, |transaction, ledger, apply, facts| {
        assert_eq!(facts[1].summaries[0].utf8_bytes, 50);
        assert!(facts[1].result.handle().is_some());
        transaction
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("roomy"))
            .publish(ledger, apply)
    })
    .unwrap()
    .expect("staged append path executed");
    assert_eq!(
        read(&arena, result.1[0]),
        format!("{original}{original}é😀é😀")
    );
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 16 + 56);
    let final_text = read(&arena, result.1[0]);
    let root = ledger
        .seal(
            heap.pages(),
            lash_kernel_state::paged::SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            },
        )
        .unwrap()
        .unwrap();
    let cold_heap = PagedHeap::fresh(Box::new(source), EditEpoch(2));
    let mut ledger = Ledger::open(root).unwrap();
    let mut cold_arena = Arena::new();
    let reopened = cold_arena.text(&final_text);
    let cold_result = Cx::prepare_text_prefix(
        &mut cold_arena,
        &ledger,
        &cold_heap,
        &[reopened, right],
        &[reopened.handle().unwrap()],
        None,
        &[(0, 1, true)],
        &[(owner, 2)],
        &[owner],
    )
    .unwrap()
    .unwrap()
    .boundary(&mut ledger, &cold_heap, |transaction, ledger, apply, _| {
        transaction
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("roomy cold append"))
            .publish(ledger, apply)
    })
    .unwrap()
    .expect("cold owner is independent of the old physical handle");
    assert_eq!(
        read(&cold_arena, cold_result.1[0]),
        format!("{final_text}é😀")
    );
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 16 + 62);
    println!(
        "prepared text consumer: 3 committed prefixes; rollback, scratch peak, stale exit and cold owner checked"
    );
}
