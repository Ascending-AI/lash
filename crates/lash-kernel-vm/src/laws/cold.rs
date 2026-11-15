//! Cold storage preserves native results, output and exact charge (FIG-6708).
//! The source oracle retries cold chunks; regions reopen canonical checkpoints.

use lash_kernel_doc::{
    Datum,
    heap::{HeapFault, PageRef},
};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use std::{cell::Cell, rc::Rc};

use super::embedder::{Embedder, Setup};
use crate::{End, Machine, Step};

/// Long texts, tuples, lists and maps nested in one another, each read by
/// a native, then by the machine.
const PROGRAM: &str = r#"
main {
  let long = text.concat("a text longer than an inline one", ", and longer still")
  let xs = [long, (long, 1), map{"k": long}]
  let m = map{"a": xs, "b": [1, 2, 3]}
  let joined = text.concat(long, long)
  let again = text.concat(joined, xs[0])
  let t = (joined, xs)
  let counted = (list.len(xs), list.len(t), list.len(m["a"]), list.len(m))
  for x in m["b"] { print (x, list.len(xs)) }
  let twice = pair.twice(list.len(m["b"]))
  return (counted, again, twice, m)
}
"#;

struct ColdPages {
    source: super::embedder::Source,
    reads: Rc<Cell<usize>>,
}

impl PageSource for ColdPages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        self.reads.set(self.reads.get() + pages.len());
        self.source.read(pages)
    }

    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        self.source.seal(body)
    }
}

#[derive(Debug, PartialEq)]
struct EndState {
    end: End,
    session: Option<lash_kernel_state::semantic::Checkpoint>,
}

fn observe(cold: bool, source: bool) -> (EndState, Vec<Datum>, u64, usize, usize) {
    let mut embedder = Embedder::with(
        PROGRAM,
        Setup {
            slice: if cold && !source { 1 } else { u64::MAX },
            ..Setup::default()
        },
    );
    let reads = Rc::new(Cell::new(0));
    if source {
        // Both runs read through the counting port, so the cold run's
        // extra reads are the chunks its natives read back. The run is
        // sealed at its first slice, a safe point of a started run.
        embedder.slice = 1;
        assert!(matches!(embedder.run(), Step::Slice));
        embedder.slice = u64::MAX;
        let root = embedder.checkpoint();
        let (program, library) = embedder.program(true);
        embedder.machine = crate::KernelMachine::open_in(
            program,
            embedder.bounds,
            &root,
            Box::new(ColdPages {
                source: embedder.source.clone(),
                reads: Rc::clone(&reads),
            }),
        )
        .unwrap();
        embedder.library = library;
        if cold {
            embedder.machine.force_cold();
        }
    }
    let mut reopens = 0;
    let mut end = loop {
        match embedder.run() {
            Step::Ended(end) => break end,
            Step::Slice if cold && !source => {
                let root = embedder.checkpoint();
                let (program, library) = embedder.program(true);
                embedder.machine = crate::KernelMachine::open_in(
                    program,
                    embedder.bounds,
                    &root,
                    Box::new(ColdPages {
                        source: embedder.source.clone(),
                        reads: Rc::clone(&reads),
                    }),
                )
                .unwrap();
                embedder.library = library;
                reopens += 1;
            }
            Step::Slice => (),
            Step::Parked(_) => panic!("the document performs nothing"),
        }
    };
    // Reopen may change allocator history and tree packing. Compare the
    // complete carry graph through the kernel's PLAN-INDEP equality domain.
    let session = if let End::Finished(finished) = &mut end {
        finished.carry.take().map(|carry| {
            lash_kernel_state::semantic::session(&carry, Box::new(embedder.source.clone())).unwrap()
        })
    } else {
        None
    };
    (
        EndState { end, session },
        embedder.world.printed,
        embedder.machine.meters().charged.expect("exact charge"),
        reads.get(),
        reopens,
    )
}

/// Cold native arguments preserve values, output and exact charge. The source
/// oracle forces each chunk to retry cold. Regions reopen their canonical
/// checkpoints in a fresh machine at every slice and read actual backing pages.
#[test]
fn a_native_that_reads_every_chunk_cold_runs_as_it_runs_warm() {
    for source in [true, false] {
        let (end, printed, charged, warm_reads, _) = observe(false, source);
        assert!(matches!(&end.end, End::Finished(_)), "{end:?}");
        let (cold_end, cold_printed, cold_charged, pages, reopens) = observe(true, source);
        assert_eq!(cold_end, end, "source={source}");
        assert_eq!(cold_printed, printed);
        assert_eq!(cold_charged, charged);
        if source {
            // Forced cold, the run reads back from its pages what the warm
            // run holds resident; a loaded object stays resident for the
            // reads after it, so the count is not one per reading call.
            assert!(
                pages > warm_reads,
                "{pages} cold page reads, {warm_reads} warm"
            );
        } else {
            assert!(reopens > 0, "regions must actually reopen cold");
            assert!(pages > 0, "reopen must read actual backing pages");
        }
    }
}

mod mixed_cycle {
    use lash_kernel_doc::Value;
    use lash_kernel_doc::arena::native::NewObject;
    use lash_kernel_state::paged::run::directory::Directory;
    use lash_kernel_state::paged::run::ids::HighWater;

    use crate::heap::Heap;
    use crate::heap::slots::SlotHeap;
    use crate::laws::embedder::Source;

    fn make(heap: &mut SlotHeap, object: NewObject) -> Value {
        heap.admit(crate::heap::slots::schedule(&object))
            .expect("room");
        heap.create(object).expect("an object")
    }

    /// Contract §6 (B4), dropped last root of a mixed cycle: `a` and `b`
    /// name each other; `b` is paged out, `a` is resident and changed
    /// since the seal, and holds a large list. Once the last root is
    /// dropped, before another seal, a collection frees `a` and its list,
    /// so an allocation that fits only then is admitted. The cold object
    /// `b` names `a`, but nothing reaches `b`: its edges are a candidate
    /// set, not a root set.
    #[test]
    fn dropped_last_root_of_a_mixed_cycle_frees_its_resident_half() {
        let pages = Source::default();
        let mut heap = SlotHeap::durable(
            u64::MAX,
            crate::heap::open::Durable {
                directory: Directory::new(Box::new(pages)),
                high_water: HighWater(0),
            },
        );
        let a = make(&mut heap, NewObject::List(vec![Value::Int(1)]));
        let b = make(&mut heap, NewObject::List(vec![a]));
        let (a_handle, b_handle) = (a.handle().expect("a"), b.handle().expect("b"));
        heap.arena_mut().list_set(a_handle, 0, b).expect("a ↔ b");
        heap.page_out(&[a]).expect("a page-out");
        heap.load(a_handle, 0, &mut |visit| visit(&a))
            .expect("a reload");
        assert!(
            heap.arena().facts(b_handle).expect("b").cold,
            "b is paged out"
        );
        assert!(
            !heap.arena().facts(a_handle).expect("a").cold,
            "a is resident"
        );

        // `a` changes after the seal: it now holds a list of 4,096 ints.
        let list = make(
            &mut heap,
            NewObject::List((0..4096).map(Value::Int).collect()),
        );
        heap.arena_mut()
            .list_set(a_handle, 1, list)
            .expect("an append");
        heap.settle(a_handle, 0);

        // The last root is dropped. The bound leaves room for half the
        // list: what is wanted fits only once `a` and its list are freed.
        let wanted = crate::heap::slots::schedule(&NewObject::List(vec![Value::Int(0); 2048]));
        heap.rebound(heap.held() + wanted / 2);
        assert!(
            heap.admit(wanted).is_err(),
            "it does not fit before a collection"
        );
        heap.collect(&mut |_| {});
        heap.admit(wanted)
            .expect("the collection freed the cycle's resident half");
        assert!(heap.arena().facts(a_handle).is_err(), "a is freed");
    }
}

/// `K-ITER-003` across a park sealed and reopened cold: a replaced key
/// keeps its place, a removed and reinserted key moves to the end and is
/// visited again, and a record's added field follows its first.
#[test]
fn map_iteration_survives_removal_and_reinsertion_across_a_cold_reopen() {
    let mut embedder = Embedder::new(
        "main {\n  let m = map{1: 10, 2: 20}\n  let r = {z: 3}\n  let first = true\n  for k in m {\n    print k\n    if first {\n      set m[1.0] = 11\n      remove m[1]\n      set m[1] = 12\n      set r.a = 4\n      set first = false\n      do perform 0(\"park\") as Any\n    }\n  }\n  return (m[1], r)\n}",
    );
    let result = loop {
        match embedder.run() {
            Step::Ended(End::Finished(finished)) => break finished.result,
            Step::Ended(other) => panic!("the run did not finish: {other:?}"),
            Step::Slice => {}
            Step::Parked(_) => {
                embedder.reopen(true);
                embedder.deliver_next();
            }
        }
    };
    let int = |n: i64| Datum::Int(n.into());
    assert_eq!(embedder.world.printed, vec![int(1), int(2), int(1)]);
    assert_eq!(
        result,
        Datum::Tuple(vec![
            int(12),
            Datum::Record(vec![("z".into(), int(3)), ("a".into(), int(4))]),
        ])
    );
}

/// Typed body-only helpers over a collection of any members: one that
/// takes it and returns, one whose body reads every member.
fn collection_helpers(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    _: &std::collections::BTreeMap<&'static str, lash_kernel_value::names::FunctionId>,
) -> Vec<(&'static str, lash_kernel_value::names::FunctionId)> {
    [
        ("cold.take_list", "xs: List(Any)", "return 7"),
        ("cold.take_map", "xs: Map(Any, Any)", "return 7"),
        ("cold.take_set", "xs: Set(Any)", "return 7"),
        (
            "cold.read_list",
            "xs: List(Any)",
            "let n = 0 for x in xs { set n = 7 } return n",
        ),
        (
            "cold.read_map",
            "xs: Map(Any, Any)",
            "let n = 0 for x in xs { set n = 7 } return n",
        ),
        (
            "cold.read_set",
            "xs: Set(Any)",
            "let n = 0 for x in xs { set n = 7 } return n",
        ),
    ]
    .into_iter()
    .map(|(name, params, body)| {
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}({params}) -> Int\nkernel 1\ncharge 1\nbody {{ {body} }}"
        ))
        .unwrap();
        (name, registry.register(definition, None).unwrap())
    })
    .collect()
}

/// The review's program (FIG-7694): a collection made before a park,
/// another after it, and the first passed to typed helper `helper`.
fn collection_program(kind: &str, helper: &str) -> String {
    let build = match kind {
        "list" => "work.fill(4000)".to_string(),
        "map" => "call build(4000)".to_string(),
        _ => "call build(4000)".to_string(),
    };
    let fill = match kind {
        "map" => "set m[i] = i",
        _ => "set m[i] = true",
    };
    let start = if kind == "map" { "map{}" } else { "set{}" };
    format!(
        "definition \"function build(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {{ let m = {start} let i = 0 while num.lt(i, n) {{ {fill} set i = num.add(i, 1) }} return m }}\"\n\
         main {{\n  let xs = {build}\n  do perform 0(0) as Any\n  let ys = work.fill(4000)\n  let answer = invoke cold.{helper}_{kind}(xs)\n  do perform 0(answer) as Any\n  return (answer, list.len(ys))\n}}\n"
    )
}

/// How the run of `text` under `memory` ends, with what it asked and
/// was charged, sealed and opened cold at its first park when `cold`,
/// and how many pages the cold machine read before its second park.
fn collection_run(
    text: &str,
    memory: u64,
    cold: bool,
) -> (End, Vec<crate::Request>, Option<u64>, usize) {
    let mut embedder = Embedder::with(
        text,
        Setup {
            bounds: crate::Bounds {
                memory,
                ..super::embedder::ROOMY
            },
            probes: Some(collection_helpers),
            ..Setup::default()
        },
    );
    let reads = Rc::new(Cell::new(0));
    let mut parks = 0;
    let mut at_second = 0;
    let end = loop {
        match embedder.run() {
            Step::Ended(end) => break end,
            Step::Slice => (),
            Step::Parked(_) => {
                parks += 1;
                if parks == 1 && cold {
                    let root = embedder.checkpoint();
                    let (program, library) = embedder.program(true);
                    match crate::KernelMachine::open_in(
                        program,
                        embedder.bounds,
                        &root,
                        Box::new(ColdPages {
                            source: embedder.source.clone(),
                            reads: Rc::clone(&reads),
                        }),
                    ) {
                        Ok(machine) => embedder.machine = machine,
                        Err(error) => panic!("open: {error:?}"),
                    }
                    embedder.library = library;
                }
                if parks == 2 {
                    at_second = reads.get();
                }
                embedder.deliver_next();
            }
        }
    };
    let charged = embedder.machine.meters().charged;
    (end, embedder.requests.clone(), charged, at_second)
}

/// The least memory under which `text` finishes, resident or `cold`.
fn least_memory(text: &str, cold: bool) -> u64 {
    let (mut low, mut high) = (0u64, super::embedder::ROOMY.memory);
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        match collection_run(text, mid, cold).0 {
            End::Finished(_) => high = mid,
            _ => low = mid,
        }
    }
    high
}

/// A typed helper's `List(Any)`, `Map(Any, Any)` or `Set(Any)` argument is
/// checked by kind only (5905f8d544, contract §2/R10: payload counts once
/// read), so a collection paged out at a park is not loaded at the call:
/// the cold run gives the resident run's result, requests and charge
/// wherever memory allows both, never refuses where the resident run does
/// not or at a lower charge, and a body that reads the collection under a
/// bound too small for it refuses at the call statement the read stands
/// in, carrying the call's charge (FIG-7694).
#[test]
fn a_kind_checked_collection_is_loaded_where_the_helper_reads_it() {
    let roomy = super::embedder::ROOMY.memory;
    let run = |text: &str, memory: u64, cold: bool| collection_run(text, memory, cold);
    // What a refusal at the call statement leaves to charge after it: the
    // list program's take call refused for its own frame. Every program
    // runs the same statements after the call.
    let list_take = collection_program("list", "take");
    let refused = run(&list_take, least_memory(&list_take, true) - 1, true);
    let at_call = match &refused.0 {
        End::Error(crate::RunError::Bound(exceeded)) => {
            assert_eq!(
                exceeded.site.as_ref().map(|site| &site.path[..]),
                Some(&[3][..])
            );
            refused.2.unwrap()
        }
        other => panic!("the take call is not what refused: {other:?}"),
    };
    let tail = run(&list_take, roomy, true).2.unwrap() - at_call;
    for kind in ["list", "map", "set"] {
        let take = collection_program(kind, "take");
        let read = collection_program(kind, "read");
        for text in [&take, &read] {
            let (warm, cold) = (run(text, roomy, false), run(text, roomy, true));
            assert!(matches!(warm.0, End::Finished(_)), "{kind}: {:?}", warm.0);
            assert_eq!(
                (&warm.0, &warm.1, warm.2),
                (&cold.0, &cold.1, cold.2),
                "{kind}"
            );
            let mut memory = 60_000;
            while memory <= 200_000 {
                let (warm, cold) = (run(text, memory, false), run(text, memory, true));
                match (&warm.0, &cold.0) {
                    (End::Finished(_), _) => {
                        assert_eq!(
                            (&warm.0, &warm.1, warm.2),
                            (&cold.0, &cold.1, cold.2),
                            "{kind} {memory}"
                        )
                    }
                    (_, End::Finished(_)) => (),
                    _ => assert!(
                        cold.2 >= warm.2,
                        "{kind} {memory}: the cold run refused earlier"
                    ),
                }
                memory += 8_000;
            }
        }
        // The take call does not load the collection: the cold run passes
        // the bound the resident run needs for both collections at once.
        let least_take = least_memory(&take, true);
        assert!(least_take < least_memory(&take, false), "{kind}");
        assert!(least_take < least_memory(&read, true), "{kind}");
        assert!(
            run(&take, roomy, true).3 < run(&read, roomy, true).3,
            "{kind}"
        );
        // Where take finishes, the read refuses at main's call statement.
        let total = run(&read, roomy, true).2.unwrap();
        match run(&read, least_take, true) {
            (End::Error(crate::RunError::Bound(exceeded)), _, Some(charged), _) => {
                assert_eq!(
                    exceeded.function.as_ref().map(|name| name.as_str()),
                    Some("main"),
                    "{kind}"
                );
                assert_eq!(
                    exceeded.site.as_deref(),
                    Some(&lash_kernel_value::sites::Site::new(
                        lash_kernel_value::sites::Unit::Main,
                        [3]
                    )),
                    "{kind}"
                );
                assert_eq!(charged, total - tail, "{kind}");
            }
            other => panic!("{kind}: {:?}", other.0),
        }
    }
}
