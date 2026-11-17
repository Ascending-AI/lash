//! A heap the machine admits always seals (cutover contract §6, paged
//! heap): every body a guest or host can build is written in bounded
//! pages whatever its size, so `seal` never reports a page past the
//! format's node bound for a well-formed heap.
//!
//! Each case builds one object past what a single 64 KiB node holds,
//! seals it, reopens the store with nothing loaded and compares the
//! reopened object, read cold, with the same object built again.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::{NewObject, Semantics};

use super::large::{Counting, fresh, make, reopened, warm, warm_within};
use crate::heap::Heap;
use crate::heap::slots::SlotHeap;

mod heads;

/// A 250-byte text, distinct for each `i`: its normal form is held inline
/// in every head that names it, the largest a head's key is.
fn wide(i: i64) -> String {
    format!("{i:0>250}")
}

fn texts(heap: &mut SlotHeap, n: i64) -> Vec<Value> {
    (0..n)
        .map(|i| make(heap, NewObject::long_text(wide(i))))
        .collect()
}

/// Members of each large integer case: past 64 KiB however compactly an
/// integer encodes (at least two bytes each here).
const M: i64 = 40_000;

/// Members of the large mixed case: past [`LARGE`] members, and past one
/// node by the heads of its texts alone.
///
/// [`LARGE`]: lash_kernel_state::paged::run::chunked::LARGE
const MIXED: i64 = 1_000;

type Build = fn(&mut SlotHeap) -> Value;

/// Every body kind that holds members, each past one node: by its member
/// count, or by the heads of the immutables its members name.
const CASES: &[(&str, Build)] = &[
    ("a tuple of 40,000 integers", |heap| {
        make(heap, NewObject::Tuple((0..M).map(Value::Int).collect()))
    }),
    ("a tuple of 256 wide texts", |heap| {
        let members = texts(heap, 256);
        make(heap, NewObject::Tuple(members))
    }),
    ("a list of 256 wide texts", |heap| {
        let members = texts(heap, 256);
        make(heap, NewObject::List(members))
    }),
    ("a set of 256 wide texts", |heap| {
        let members = texts(heap, 256);
        make(heap, NewObject::Set(members))
    }),
    ("a map of 256 wide texts to wide texts", |heap| {
        let keys = texts(heap, 256);
        let values = texts(heap, 256);
        let values: Vec<Value> = values.into_iter().rev().collect();
        make(heap, NewObject::Map(keys.into_iter().zip(values).collect()))
    }),
    ("a record of 256 wide fields", |heap| {
        let keys = texts(heap, 256);
        let pairs = keys
            .into_iter()
            .enumerate()
            .map(|(i, key)| (key, Value::Int(i as i64)))
            .collect();
        make(heap, NewObject::Record(pairs))
    }),
    ("a tuple of two large tuples", |heap| {
        let left = make(heap, NewObject::Tuple((0..M).map(Value::Int).collect()));
        let right = make(
            heap,
            NewObject::Tuple((0..M).rev().map(Value::Int).collect()),
        );
        make(heap, NewObject::Tuple(vec![left, right]))
    }),
    ("a large tuple of texts and tuples", |heap| {
        let members = (0..MIXED)
            .map(|i| match i % 3 {
                0 => make(heap, NewObject::long_text(wide(i))),
                1 => make(heap, NewObject::Tuple(vec![Value::Int(i), Value::Null])),
                _ => Value::Int(i),
            })
            .collect();
        make(heap, NewObject::Tuple(members))
    }),
];

/// Contract §6: each case seals, and reopens cold holding what it held.
#[test]
fn every_body_past_the_node_bound_seals_and_reopens_equal() {
    let mut failed = Vec::new();
    for (case, build) in CASES {
        let store = Counting::default();
        let mut heap = fresh(&store);
        let object = build(&mut heap);
        let root = match heap.seal(&[object]) {
            Ok(root) => root,
            Err(error) => {
                failed.push(format!("{case}: the seal failed: {error:?}"));
                continue;
            }
        };
        let (mut heap, cold) = reopened(&store, &root);
        let again = build(&mut heap);
        // Every chunk and every cold member once, and each join.
        let loads = 2 * M as usize + 16;
        let equal = warm_within(&mut heap, &[cold, again], loads, |heap| {
            heap.arena().semantics().equal(&cold, &again)
        });
        if !equal {
            failed.push(format!("{case}: the reopened object differs"));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Bytes of each wide body: past one 64 KiB node by itself.
const WIDE: usize = 65_600;

/// Decimal digits of each big integer: its magnitude passes 64 KiB
/// (65,536 · 8 / log₂ 10 ≈ 157,826 digits).
const DIGITS: usize = 158_000;

fn big(digits: usize) -> lash_kernel_doc::Integer {
    lash_kernel_doc::Integer::parse(&format!("7{}", "3".repeat(digits - 1))).expect("an integer")
}

fn host_handle(heap: &mut SlotHeap, id: String) -> Value {
    heap.arena_mut().host_handle(lash_kernel_doc::Handle {
        kind: "file".into(),
        id,
    })
}

/// One fixture for each row of the seal inventory (`seal-design.md`), the
/// smallest past the old page bound, that a host freezes into the heap.
const FORMS: &[(&str, Build)] = &[
    ("tuple row: a tuple of 40,000 integers", |heap| {
        make(heap, NewObject::Tuple((0..M).map(Value::Int).collect()))
    }),
    (
        "tuple row: a tuple of 7,000 members of every kind, over two index pages",
        |heap| {
            let long = make(heap, NewObject::long_text("l".repeat(300)));
            let int = make(heap, NewObject::BigInt(big(40)));
            let inner = make(heap, NewObject::Tuple(vec![Value::Int(1), long]));
            let list = make(heap, NewObject::List(vec![Value::Int(2)]));
            let error = heap
                .error("kind", "message", Value::Null)
                .expect("an error");
            let handle = host_handle(heap, "h".into());
            let kinds = [
                Value::Int(3),
                Value::Null,
                Value::Bool(true),
                long,
                int,
                inner,
                list,
                error,
                handle,
            ];
            let members = (0..7_000).map(|at| kinds[at % kinds.len()]).collect();
            make(heap, NewObject::Tuple(members))
        },
    ),
    (
        "list row: four distinct host handles of 20,000-byte ids",
        |heap| {
            let members = (0..4)
                .map(|i| host_handle(heap, format!("{i}{}", "h".repeat(19_999))))
                .collect();
            make(heap, NewObject::List(members))
        },
    ),
    ("table row: a map of 256 wide-text keys", |heap| {
        let keys = texts(heap, 256);
        make(
            heap,
            NewObject::Map(keys.into_iter().map(|key| (key, Value::Null)).collect()),
        )
    }),
    (
        "error row: an error whose message and data are each past a node",
        |heap| {
            let data = make(heap, NewObject::Bytes(vec![9u8; WIDE].into()));
            heap.error("wide", &"m".repeat(WIDE), data)
                .expect("an error")
        },
    ),
    ("text row: a text past a node", |heap| {
        make(heap, NewObject::long_text("t".repeat(WIDE)))
    }),
    ("bytes row: bytes past a node", |heap| {
        make(heap, NewObject::Bytes(vec![7u8; WIDE].into()))
    }),
    ("scalar row: an integer past a node", |heap| {
        make(heap, NewObject::BigInt(big(DIGITS)))
    }),
    ("scalar row: a timestamp past a node", |heap| {
        make(heap, NewObject::Timestamp(big(DIGITS)))
    }),
    (
        "scalar row: a host handle whose id is past a node",
        |heap| host_handle(heap, "i".repeat(WIDE)),
    ),
];

/// Contract §2, admission before materialization: a cold immutable's
/// load admits what it will hold, its decoded value and the staging its
/// payload is copied through, before it reads its payload. A big integer
/// and a long host handle, each a payload tree, and a long text, reopened
/// under a bound with room for 1 KiB more than the heap holds: the load
/// that would hold the value refuses as `Memory` at that bound, as the
/// text's does, and the heap holds what it held. What the refused load
/// staged is bounded by the pages it read, at most its row's and one
/// payload node page (the staging constant: the text reads the leaf it
/// then refuses), never the payload.
#[test]
fn a_cold_payload_is_admitted_before_its_pages_are_read() {
    use lash_kernel_state::paged::run::directory::Directory;
    use lash_kernel_state::paged::run::ids::ObjectId;

    use crate::heap::LoadError;

    let cases: [(&str, Build); 3] = [
        ("a big integer", |heap| {
            make(heap, NewObject::BigInt(big(DIGITS)))
        }),
        ("a long host handle", |heap| {
            host_handle(heap, "i".repeat(WIDE))
        }),
        ("a long text", |heap| {
            make(heap, NewObject::long_text("t".repeat(WIDE)))
        }),
    ];
    let mut failed = Vec::new();
    for (case, build) in cases {
        let store = Counting::default();
        let mut heap = fresh(&store);
        let value = build(&mut heap);
        let root = heap.seal(&[value]).expect("a seal");
        let (mut heap, cold) = reopened(&store, &root);
        let handle = cold.handle().expect("an object");
        let id = ObjectId(
            heap.arena()
                .facts(handle)
                .expect("live")
                .entity()
                .expect("an id")
                .ordinal,
        );
        let directory = Directory::open(root.directory, Box::new(store.clone()));
        let (_, depth, _) = store.during(|| directory.get(id).expect("a read").expect("a row"));
        let bound = heap.held() + 1024;
        heap.rebound(bound);
        let mut refused = None;
        for _ in 0..8 {
            match crate::data::copy_out(heap.arena(), usize::MAX, &cold) {
                Ok(_) => break,
                Err(crate::data::CopyError::Cold { object, chunk }) => {
                    let held = heap.held();
                    let (loaded, reads, _) =
                        store.during(|| heap.load(object, chunk, &mut |visit| visit(&cold)));
                    if let Err(error) = loaded {
                        refused = Some((error, held, heap.held(), reads));
                        break;
                    }
                }
                Err(other) => panic!("{case}: a copy failed: {other:?}"),
            }
        }
        match refused {
            Some((LoadError::Memory(refusal), before, after, reads)) => {
                if refusal.bound != bound {
                    failed.push(format!("{case}: refused at {}, not {bound}", refusal.bound));
                }
                if before != after {
                    failed.push(format!(
                        "{case}: the refusal moved the holding {before} to {after}"
                    ));
                }
                if reads > depth + 1 {
                    failed.push(format!(
                        "{case}: {reads} pages read by the refused load, past its row's {depth} and one node"
                    ));
                }
            }
            other => failed.push(format!("{case}: the load answered {other:?}")),
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Contract §2, admission before materialization, for a closure: its row
/// names its code (capture names and site) and its captures by their pages
/// once they outgrow the row. A closure of 300 captures, each named by a
/// 300-byte name, so both its code and its captures are paged, reopened
/// under a bound with room for 1 KiB more than the heap holds: its load
/// refuses as `Memory` at that bound, the heap holds what it held, and the
/// refused load read at most its row's pages and one node, never the code
/// or the captures.
#[test]
fn a_cold_closure_is_admitted_before_its_code_and_captures_are_read() {
    use std::sync::Arc;

    use lash_kernel_doc::arena::ClosureIdentity;
    use lash_kernel_doc::linked_site::LinkedSite;
    use lash_kernel_state::paged::run::directory::Directory;
    use lash_kernel_state::paged::run::ids::ObjectId;
    use lash_kernel_value::Name;
    use lash_kernel_value::content_ids::LinkId;
    use lash_kernel_value::names::{CallableId, FunctionId};
    use lash_kernel_value::sites::{Site, Unit};

    use crate::heap::LoadError;

    const CAPTURES: usize = 300;
    let store = Counting::default();
    let mut heap = fresh(&store);
    let cells: Vec<_> = (0..CAPTURES).map(|_| heap.cell(Value::Int(1))).collect();
    let link = LinkId::from_bytes([4; 32]);
    let identity = Arc::new(ClosureIdentity {
        capture_names: (0..CAPTURES)
            .map(|i| Name::new(format!("{i:0>300}")))
            .collect(),
        callable: CallableId {
            link,
            function: FunctionId::from_bytes([5; 32]),
        },
        site: LinkedSite {
            link,
            site: Site::new(Unit::Main, [1]),
        },
    });
    let closure = Value::Closure(heap.closure(identity, cells));
    let root = heap.seal(&[closure]).expect("a seal");
    let (mut heap, cold) = reopened(&store, &root);
    let handle = cold.handle().expect("an object");
    let id = ObjectId(
        heap.arena()
            .facts(handle)
            .expect("live")
            .entity()
            .expect("an id")
            .ordinal,
    );
    let directory = Directory::open(root.directory, Box::new(store.clone()));
    let (_, depth, _) = store.during(|| directory.held(id).expect("a read").expect("a row"));
    // The collection a refused admission runs first traces the closure's
    // edges through its edge pages, never its code or captures: it runs
    // once here, so what the load itself reads is measured.
    heap.collect(&mut |visit| visit(&cold));
    let bound = heap.held() + 1024;
    heap.rebound(bound);
    let before = heap.held();
    let (loaded, reads, _) = store.during(|| heap.load(handle, 0, &mut |visit| visit(&cold)));
    match loaded {
        Err(LoadError::Memory(refusal)) => {
            assert_eq!(refusal.bound, bound);
            assert_eq!(heap.held(), before, "the refusal moved the holding");
            assert!(
                reads <= depth + 1,
                "{reads} pages read by the refused load, past its row's {depth} and one node"
            );
        }
        other => panic!("the load answered {other:?}"),
    }
}

/// What `value` copies out to the host as, every page it reaches loaded
/// first.
fn copied(heap: &mut SlotHeap, roots: &[Value], value: Value) -> lash_kernel_doc::Datum {
    for _ in 0..=2 * M {
        match crate::data::copy_out(heap.arena(), usize::MAX, &value) {
            Ok(datum) => return datum,
            Err(crate::data::CopyError::Cold { object, chunk }) => heap
                .load(object, chunk, &mut |visit| {
                    roots.iter().for_each(&mut *visit)
                })
                .expect("a load"),
            Err(other) => panic!("a copy failed: {other:?}"),
        }
    }
    panic!("a copy asked for more loads than its object has chunks");
}

/// The seal rule (cutover contract §6): a heap the machine admits always
/// seals. For every row of the seal inventory, its smallest fixture past
/// the old page bound seals with no fault, and reopens equal: read cold,
/// against the same object built again, and warm, every page loaded and
/// copied out, against the host's copy of the object built again.
#[test]
fn seal_rule_every_form() {
    let mut failed = Vec::new();
    for (case, build) in FORMS {
        let store = Counting::default();
        let mut heap = fresh(&store);
        let object = build(&mut heap);
        let root = match heap.seal(&[object]) {
            Ok(root) => root,
            Err(error) => {
                failed.push(format!("{case}: the seal failed: {error:?}"));
                continue;
            }
        };
        let (mut heap, cold) = reopened(&store, &root);
        let again = build(&mut heap);
        let loads = 2 * M as usize + 16;
        if !warm_within(&mut heap, &[cold, again], loads, |heap| {
            heap.arena().semantics().equal(&cold, &again)
        }) {
            failed.push(format!("{case}: the object reopened cold differs"));
        }
        let (mut heap, warm) = reopened(&store, &root);
        let warmed = copied(&mut heap, &[warm], warm);
        let mut built = fresh(&Counting::default());
        let again = build(&mut built);
        if warmed != copied(&mut built, &[again], again) {
            failed.push(format!("{case}: the object reopened warm differs"));
        }
    }
    if let Err(error) = session_row() {
        failed.push(format!("session row: {error}"));
    }
    if let Err(error) = run_record_row() {
        failed.push(format!("run record row: {error}"));
    }
    if let Err(error) = closure_row() {
        failed.push(format!("closure row: {error}"));
    }
    for captures in CAPTURES {
        if let Err(error) = closure_captures(captures) {
            failed.push(format!("closure of {captures} captures: {error}"));
        }
    }
    failed.extend(deep_rows());
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Members of each deep sequence: past what one level of index pages
/// below the row holds (a tuple's full pages: 96 of 96 chunks of 64
/// members, 589,824; a list's 72-reference fill: 442,368), so its index
/// takes two levels below the row (cutover contract §6 index rule).
const DEEP: i64 = 2_400_000;

/// The deep rows of the inventory: a list and a tuple of [`DEEP`]
/// integers, built at the heap, sealed, and reopened cold. A point read at
/// the far end loads the row's `d` directory pages, one index page per
/// level below the row and the chunk; then every chunk loaded, the object
/// equals the same object built again.
fn deep_rows() -> Vec<String> {
    use lash_kernel_state::paged::run::directory::Directory;
    use lash_kernel_state::paged::run::ids::ObjectId;
    use lash_kernel_state::paged::run::objects::Record;

    use super::large::{int, item};

    let forms: [(&str, Build); 2] = [
        ("deep list row: a list of 2,400,000 integers", |heap| {
            make(heap, NewObject::List((0..DEEP).map(Value::Int).collect()))
        }),
        ("deep tuple row: a tuple of 2,400,000 integers", |heap| {
            make(heap, NewObject::Tuple((0..DEEP).map(Value::Int).collect()))
        }),
    ];
    let mut failed = Vec::new();
    for (case, build) in forms {
        let store = Counting::default();
        let mut heap = fresh(&store);
        let object = build(&mut heap);
        let root = match heap.seal(&[object]) {
            Ok(root) => root,
            Err(error) => {
                failed.push(format!("{case}: the seal failed: {error:?}"));
                continue;
            }
        };
        drop(heap);
        let (mut heap, cold) = reopened(&store, &root);
        let handle = cold.handle().expect("an object");
        let id = ObjectId(
            heap.arena()
                .facts(handle)
                .expect("live")
                .entity()
                .expect("an id")
                .ordinal,
        );
        let directory = Directory::open(root.directory, Box::new(store.clone()));
        let (row, depth, _) = store.during(|| directory.get(id).expect("a read").expect("a row"));
        let Record::Chunked(chunked) = &row.record else {
            failed.push(format!("{case}: the row is not chunked"));
            continue;
        };
        let levels = u64::from(chunked.index.height());
        if levels != 2 {
            failed.push(format!(
                "{case}: its index is {levels} levels below the row, not 2"
            ));
        }
        let (value, reads, _) = store.during(|| item(&mut heap, &[cold], cold, DEEP - 1));
        if int(value) != Some(DEEP - 1) {
            failed.push(format!("{case}: the far end reads {value:?}"));
        }
        if reads != depth + 1 + levels {
            failed.push(format!(
                "{case}: a cold far-end read loads {reads} pages, not d + 1 + L = {depth} + 1 + {levels}"
            ));
        }
        for chunk in 0..chunked.chunks() {
            let chunk = u32::try_from(chunk).expect("a chunk number");
            heap.load(handle, chunk, &mut |visit| visit(&cold))
                .expect("a chunk");
        }
        let again = build(&mut heap);
        if !warm_within(&mut heap, &[cold, again], 16, |heap| {
            heap.arena().semantics().equal(&cold, &again)
        }) {
            failed.push(format!("{case}: the object reopened cold differs"));
        }
    }
    failed
}

/// The session row of the inventory: a binding's name and a saved
/// function's name and description, each past a node, frozen by the host
/// into a session, sealed, reopened cold, and carried warm through a cell
/// that reads and rewrites the session.
fn session_row() -> Result<(), String> {
    use lash_kernel_state::kernel_internals::tree::Access;
    use lash_kernel_state::paged::SessionRoot;
    use lash_kernel_state::paged::run::session::{self, RunFunction, RunSession};
    use lash_kernel_value::names::Name;

    let source = super::embedder::Source::default();
    let mut heap = SlotHeap::durable(
        u64::MAX,
        crate::heap::open::Durable {
            directory: lash_kernel_state::paged::run::directory::Directory::new(Box::new(
                source.clone(),
            )),
            high_water: lash_kernel_state::paged::run::ids::HighWater(0),
        },
    );
    let function = make(
        &mut heap,
        NewObject::Function(lash_kernel_value::CallableId {
            link: lash_kernel_value::LinkId::from_bytes([4; 32]),
            function: lash_kernel_value::FunctionId::from_bytes([5; 32]),
        }),
    );
    let bound = make(&mut heap, NewObject::List(vec![Value::Int(1)]));
    let name = Name::new(format!("b{}", "n".repeat(WIDE)));
    let saved = RunFunction {
        name: Name::new(format!("f{}", "g".repeat(WIDE))),
        description: vec![b'd'; WIDE],
    };
    let root = heap
        .seal(&[bound, function])
        .map_err(|error| format!("{error:?}"))?;
    let page = session::write(
        &Access::new(Box::new(source.clone())),
        &RunSession {
            heap: root,
            names: vec![name.clone()],
            functions: vec![saved.clone()],
        },
        None,
    )
    .map_err(|error| format!("the session seal failed: {error:?}"))?;
    let (cold, _) = session::open(Box::new(source.clone()), page)
        .map_err(|error| format!("the cold reopen failed: {error:?}"))?;
    if cold.names != [name.clone()] || cold.functions != [saved.clone()] {
        return Err("the session reopened cold differs".into());
    }
    let end = super::session::cell(
        "main {\n  return 0\n}",
        super::session::start(Some(SessionRoot::run(page)), Vec::new()),
        &source,
    );
    let carried = end
        .carry
        .as_ref()
        .ok_or("the cell carries no session")?
        .page();
    let (warm, _) = session::open(Box::new(source.clone()), carried)
        .map_err(|error| format!("the carried reopen failed: {error:?}"))?;
    if warm.names != [name] || warm.functions != [saved] {
        return Err("the session carried warm differs".into());
    }
    Ok(())
}

/// The run record row of the inventory: a guest parked 4,000 calls deep in
/// a function of 20 nested conditionals and 512 locals, its encoded run
/// far past one node, sealed and reopened cold, and sealed and run on
/// warm, each ending as the same run never sealed does.
fn run_record_row() -> Result<(), String> {
    use super::embedder::{Embedder, ROOMY, Setup};
    use crate::Step;

    let mut source = String::from("main { let down = fn(self, n) {");
    for _ in 0..20 {
        source.push_str("if true {");
    }
    for i in 0..512 {
        source.push_str(&format!("let v{i} = {i}\n"));
    }
    source.push_str(
        "if num.lt(n, 1) { return perform 0(\"bottom\") as Any } \
         let answer = apply self(self, num.add(n, -1)) return answer",
    );
    for _ in 0..20 {
        source.push('}');
    }
    source.push_str("return 0 } return apply down(down, 4000) }");
    let parked = || {
        let mut embedder = Embedder::with(
            &source,
            Setup {
                bounds: ROOMY,
                ..Setup::default()
            },
        );
        match embedder.run() {
            Step::Parked(_) => Ok(embedder),
            other => Err(format!("the run did not park: {other:?}")),
        }
    };
    let expected = parked()?.run_to_end(&[]);
    let mut cold = parked()?;
    cold.try_reopen(true)
        .map_err(|error| format!("the cold reopen failed: {error}"))?;
    if cold.run_to_end(&[]) != expected {
        return Err("the run reopened cold ends otherwise".into());
    }
    let mut warm = parked()?;
    let root = warm
        .try_checkpoint()
        .map_err(|error| format!("the seal failed: {error:?}"))?;
    // Past what the run root's own top node names: its segment index has
    // a level of index pages (cutover contract §6 index rule).
    let access =
        lash_kernel_state::kernel_internals::tree::Access::new(Box::new(warm.source.clone()));
    let page = access
        .read(root.heap)
        .map_err(|error| format!("the run root reads: {error:?}"))?;
    let run: lash_kernel_state::paged::run::continuation::RunPage =
        lash_kernel_value::tokens::decode_body(&page.tokens)
            .map_err(|error| format!("the run root decodes: {error}"))?;
    let segments = run.segments.leaves();
    if segments <= 96 || run.segments.height() == 0 {
        return Err(format!(
            "the run is {segments} segments at index height {}, not past one node",
            run.segments.height()
        ));
    }
    if warm.run_to_end(&[]) != expected {
        return Err("the run sealed warm ends otherwise".into());
    }
    Ok(())
}

/// The closure row: a closure capturing a binding whose name is past one
/// payload leaf, which the run holds over a park. Its code (capture names,
/// callable, site) is past the inline bound, so its row names it by a
/// payload; the run seals, and reopened cold or sealed warm it ends as the
/// run that never parked.
fn closure_row() -> Result<(), String> {
    use super::embedder::Embedder;
    use crate::Step;

    let name = "n".repeat(65_600);
    let source = format!(
        "main {{ let {name} = 7 let f = fn() {{ return {name} }} \
         let pause = perform 0(\"pause\") as Any return apply f() }}"
    );
    let parked = || {
        let mut embedder = Embedder::new(&source);
        match embedder.run() {
            Step::Parked(_) => Ok(embedder),
            other => Err(format!("the run did not park: {other:?}")),
        }
    };
    let expected = parked()?.run_to_end(&[]);
    let mut cold = parked()?;
    cold.try_reopen(true)
        .map_err(|error| format!("the cold reopen failed: {error}"))?;
    if cold.run_to_end(&[]) != expected {
        return Err("the run reopened cold ends otherwise".into());
    }
    let mut warm = parked()?;
    warm.try_checkpoint()
        .map_err(|error| format!("the seal failed: {error:?}"))?;
    if warm.run_to_end(&[]) != expected {
        return Err("the run sealed warm ends otherwise".into());
    }
    Ok(())
}

/// Capture counts of the closure captures row, each one past a boundary
/// of its form: the last inline count ([`LARGE`]) and the first paged one;
/// one edge page full ([`EDGE_REFS`]) and two; one chunk index page full
/// (96 chunks of 64) and two; the 20,000 the seal once refused; and the
/// most this program captures: `main`'s 65,535 locals (the core's limit)
/// less `f` and `pause`, in the 1,024 chunks the most one function
/// captures (`captures of one function`, u16::MAX) takes too.
///
/// [`LARGE`]: lash_kernel_state::paged::run::chunked::LARGE
/// [`EDGE_REFS`]: lash_kernel_state::paged::run::chunked::EDGE_REFS
const CAPTURES: [usize; 8] = [256, 257, 1024, 1025, 6144, 6145, 20_000, 65_533];

/// The closure captures row: a closure capturing `captures` bindings,
/// held over a park, which returns them as a list in capture order. The
/// run seals, and reopened cold or sealed warm it ends as the run that
/// never parked.
fn closure_captures(captures: usize) -> Result<(), String> {
    use super::embedder::Embedder;
    use crate::Step;

    let mut source = String::from("main { ");
    for i in 0..captures {
        source.push_str(&format!("let c{i} = {i} "));
    }
    source.push_str("let f = fn() { return [");
    for i in 0..captures {
        source.push_str(&format!("c{i}, "));
    }
    source.push_str("] } let pause = perform 0(\"pause\") as Any return apply f() }");
    let parked = || {
        let mut embedder = Embedder::new(&source);
        match embedder.try_run() {
            Ok(Step::Parked(_)) => Ok(embedder),
            other => Err(format!("the run did not park: {other:?}")),
        }
    };
    let expected = parked()?.run_to_end(&[]);
    let mut cold = parked()?;
    cold.try_reopen(true)
        .map_err(|error| format!("the cold reopen failed: {error}"))?;
    if cold.run_to_end(&[]) != expected {
        return Err("the run reopened cold ends otherwise".into());
    }
    let mut warm = parked()?;
    warm.try_checkpoint()
        .map_err(|error| format!("the seal failed: {error:?}"))?;
    if warm.run_to_end(&[]) != expected {
        return Err("the run sealed warm ends otherwise".into());
    }
    Ok(())
}

/// What a substitution case builds: the root the read starts from, the
/// object whose row is replaced, and the object of the same shape whose
/// valid row replaces it. Each object is a root of the seal.
type Swap = fn(&mut SlotHeap) -> [Value; 3];

/// A read a case makes of its reopened root.
#[derive(Clone, Copy)]
enum Read {
    /// The tuple's first member.
    Item,
    /// The tuple against the same tuple built again.
    Equal,
    /// The object copied out whole.
    Copy,
    /// The first member of the tuple's first member.
    Nested,
}

/// A text of 9,000 `fill`s: a payload tree past one leaf.
fn text(heap: &mut SlotHeap, fill: char) -> Value {
    make(heap, NewObject::long_text(fill.to_string().repeat(9_000)))
}

fn ints(heap: &mut SlotHeap, from: i64) -> Value {
    make(
        heap,
        NewObject::Tuple((from..from + 512).map(Value::Int).collect()),
    )
}

/// A tuple past one index level below its row: 600,000 integers, more
/// than 96 index pages of 96 chunks of 64.
fn deep_ints(heap: &mut SlotHeap, from: i64) -> Value {
    make(
        heap,
        NewObject::Tuple((from..from + 600_000).map(Value::Int).collect()),
    )
}

/// What of the row a case substitutes.
#[derive(Clone, Copy)]
enum Substitute {
    /// The whole row, for the other object's.
    Row,
    /// The index top the row embeds, for the other row's.
    Top,
    /// The page under the first index page the row names, for the other
    /// tuple's page at the same place, its parent otherwise unchanged.
    Second,
    /// The whole row, for the row the other object had at an earlier seal.
    Stale,
    /// In the root tuple's first chunk, the head of its member (the
    /// object replaced) keeps its key and takes the other tuple's root,
    /// and the member's row is the other tuple's. Only the root tuple and
    /// the other are roots of the seal.
    NestedRoot,
    /// In the root holder's first chunk, one field of its member's head
    /// is forged; the member's row and every other page are kept. Only
    /// the holder is a root of the seal. The function forges the field,
    /// `false` when the head is of another kind.
    NestedHead(fn(&mut lash_kernel_state::paged::run::objects::Head) -> bool),
    /// As [`Self::NestedHead`], the member's head held in the root
    /// holder's inline row, a closure's in the row of the cell it
    /// captures.
    InlineHead(fn(&mut lash_kernel_state::paged::run::objects::Head) -> bool),
}

/// The root tuple of a [`Substitute::NestedHead`] case: 513 of the
/// immutable `member`, so its first chunk names the member's head.
fn nested_of(heap: &mut SlotHeap, member: Value) -> [Value; 3] {
    let outer = make(heap, NewObject::Tuple(vec![member; 513]));
    [outer, member, member]
}

/// A list of 512 integers from `from`.
fn list_ints(heap: &mut SlotHeap, from: i64) -> Value {
    make(
        heap,
        NewObject::List((from..from + 512).map(Value::Int).collect()),
    )
}

/// A tuple of 513 `fill`s.
fn filled(heap: &mut SlotHeap, fill: i64) -> Value {
    make(heap, NewObject::Tuple(vec![Value::Int(fill); 513]))
}

/// Every partial load of an immutable checks the pages it consumes
/// against the head that names it (cutover contract §6): for each kind
/// of immutable a row can stand for, a store that answers a valid row of
/// another object of the same shape under an unchanged head fails the
/// first read that consumes it with `Store(Corrupt)`, before anything of
/// the substituted row is installed.
#[test]
fn immutable_head_first_read() {
    use lash_kernel_state::paged::run::directory::Directory;
    use lash_kernel_state::paged::run::objects::Record;

    use crate::heap::LoadError;

    use lash_kernel_state::kernel_internals::tree::Access;
    use lash_kernel_state::paged::PageKind;
    use lash_kernel_state::paged::run::chunked::Placement;
    use lash_kernel_state::paged::run::index::{Index, Part};

    let cases: &[(&str, Swap, Read, Substitute)] = &[
        (
            "tuple point read",
            |heap| {
                let a = ints(heap, 0);
                [a, a, ints(heap, 1000)]
            },
            Read::Item,
            Substitute::Row,
        ),
        (
            "tuple point read, index path",
            |heap| {
                let a = ints(heap, 0);
                [a, a, ints(heap, 1000)]
            },
            Read::Item,
            Substitute::Top,
        ),
        (
            "tuple point read, second index level",
            |heap| {
                let a = deep_ints(heap, 0);
                [a, a, deep_ints(heap, 1_000_000)]
            },
            Read::Item,
            Substitute::Second,
        ),
        (
            "tuple point read, another list's row",
            |heap| {
                let a = ints(heap, 0);
                [a, a, list_ints(heap, 1000)]
            },
            Read::Item,
            Substitute::Row,
        ),
        (
            "tuple point read, a stale earlier list row",
            |heap| {
                let a = ints(heap, 0);
                [a, a, list_ints(heap, 1000)]
            },
            Read::Item,
            Substitute::Stale,
        ),
        (
            "tuple point read, a text's row",
            |heap| {
                let a = ints(heap, 0);
                [a, a, text(heap, 'b')]
            },
            Read::Item,
            Substitute::Row,
        ),
        (
            "text copy, a list's row",
            |heap| {
                let a = text(heap, 'a');
                [a, a, list_ints(heap, 1000)]
            },
            Read::Copy,
            Substitute::Row,
        ),
        (
            "text copy, a big integer's row",
            |heap| {
                let a = text(heap, 'a');
                let digits = format!("7{}", "3".repeat(19_999));
                let int = lash_kernel_doc::Integer::parse(&digits).expect("an integer");
                [a, a, make(heap, NewObject::BigInt(int))]
            },
            Read::Copy,
            Substitute::Row,
        ),
        (
            "big integer copy, a tuple's row",
            |heap| {
                let digits = format!("7{}", "3".repeat(19_999));
                let int = lash_kernel_doc::Integer::parse(&digits).expect("an integer");
                let a = make(heap, NewObject::BigInt(int));
                [a, a, ints(heap, 1000)]
            },
            Read::Copy,
            Substitute::Row,
        ),
        (
            "nested tuple, its key kept and its root substituted",
            |heap| {
                let a = filled(heap, 10);
                let b = filled(heap, 11);
                let outer = make(heap, NewObject::Tuple(vec![a; 513]));
                [outer, a, b]
            },
            Read::Nested,
            Substitute::NestedRoot,
        ),
        (
            "tuple memberwise equality",
            |heap| {
                let a = ints(heap, 0);
                [a, a, ints(heap, 1000)]
            },
            Read::Equal,
            Substitute::Row,
        ),
        (
            "payload leaf",
            |heap| {
                let a = text(heap, 'a');
                [a, a, text(heap, 'b')]
            },
            Read::Copy,
            Substitute::Row,
        ),
        (
            "scalar leaf",
            |heap| {
                let digits = |fill: &str| {
                    lash_kernel_doc::Integer::parse(&format!("7{}", fill.repeat(19_999)))
                        .expect("an integer")
                };
                let a = make(heap, NewObject::BigInt(digits("3")));
                [a, a, make(heap, NewObject::BigInt(digits("4")))]
            },
            Read::Copy,
            Substitute::Row,
        ),
        (
            "error field",
            |heap| {
                let a = heap
                    .error("kind", "message a", Value::Null)
                    .expect("an error");
                let b = heap
                    .error("kind", "message b", Value::Null)
                    .expect("an error");
                [a, a, b]
            },
            Read::Copy,
            Substitute::Row,
        ),
        (
            "cold map key",
            |heap| {
                let a = text(heap, 'a');
                let map = make(heap, NewObject::Map(vec![(a, Value::Int(1))]));
                [map, a, text(heap, 'b')]
            },
            Read::Copy,
            Substitute::Row,
        ),
        (
            "cold set member",
            |heap| {
                let a = text(heap, 'a');
                let set = make(heap, NewObject::Set(vec![a]));
                [set, a, text(heap, 'b')]
            },
            Read::Copy,
            Substitute::Row,
        ),
    ];
    let mut failed = Vec::new();
    for (case, build, read, substitute) in cases {
        let store = Counting::default();
        let mut heap = fresh(&store);
        let roots = build(&mut heap);
        if let Substitute::NestedRoot | Substitute::NestedHead(_) | Substitute::InlineHead(_) =
            substitute
        {
            match nested(&store, &mut heap, &roots, *substitute) {
                Ok(root) => {
                    let (mut heap, cold) = reopened(&store, &root);
                    let again = build(&mut heap)[0];
                    match first_read(&mut heap, case, *read, [cold, again]) {
                        Ok(Some(LoadError::Store(lash_kernel_doc::heap::HeapFault::Corrupt {
                            ..
                        }))) => {}
                        Err(error) => failed.push(error),
                        other => failed.push(format!("{case}: the first read answered {other:?}")),
                    }
                }
                Err(error) => failed.push(format!("{case}: {error}")),
            }
            continue;
        }
        let early = match substitute {
            Substitute::Stale => {
                let early = heap.seal(&roots).expect("an early seal");
                let list = roots[2].handle().expect("a list");
                warm(&mut heap, &roots, |heap| {
                    heap.arena_mut().list_set(list, 0, Value::Int(-1))
                });
                Some(early)
            }
            _ => None,
        };
        let mut root = heap.seal(&roots).expect("a seal");
        let target = |at: usize| root.roots[at].id().expect("an object");
        let (a, b) = (target(1), target(2));
        let mut directory = Directory::open(root.directory, Box::new(store.clone()));
        let other = match &early {
            Some(early) => Directory::open(early.directory, Box::new(store.clone()))
                .get(b)
                .expect("a read")
                .expect("an earlier row"),
            None => directory.get(b).expect("a read").expect("a row"),
        };
        let row = match substitute {
            Substitute::Row
            | Substitute::Stale
            | Substitute::NestedRoot
            | Substitute::NestedHead(_)
            | Substitute::InlineHead(_) => other,
            Substitute::Top | Substitute::Second => {
                let mut row = directory.get(a).expect("a read").expect("a row");
                match (&mut row.record, &other.record) {
                    (Record::Chunked(mine), Record::Chunked(theirs)) => {
                        if matches!(substitute, Substitute::Top) {
                            mine.index.clone_from(&theirs.index);
                        } else {
                            // The first index page is rewritten to name the
                            // other tuple's page below it in its own page's
                            // place, every content id kept: the first page
                            // passes its parent, and the read reaches the
                            // substituted page.
                            let access = Access::new(Box::new(store.clone()));
                            let first = |index: &Index<_>| match index {
                                Index::Nodes { height: 2, refs } => Some(refs[0]),
                                _ => None,
                            };
                            let (Some(mine_first), Some(theirs_first)) =
                                (first(&mine.index), first(&theirs.index))
                            else {
                                failed.push(format!("{case}: the tuples are not two levels deep"));
                                continue;
                            };
                            let parts =
                                |part: Part<Option<Placement>>| -> Vec<Part<Option<Placement>>> {
                                    let page = access.read(part.page).expect("an index page");
                                    let mut parts: Vec<Part<Option<Placement>>> =
                                        lash_kernel_value::tokens::decode_body(&page.tokens)
                                            .expect("index parts");
                                    for part in &mut parts {
                                        lash_kernel_state::kernel_internals::tree::grant(
                                            &mut part.page,
                                            &page.children,
                                        )
                                        .expect("a granted part");
                                    }
                                    parts
                                };
                            let mut forged = parts(mine_first);
                            forged[0].page = parts(theirs_first)[0].page;
                            let children = forged.iter().map(|part| part.page).collect();
                            let page = access
                                .seal(
                                    PageKind::SEQUENCE_INDEX,
                                    lash_kernel_value::tokens::encode_body(&forged),
                                    children,
                                )
                                .expect("a page");
                            if let Index::Nodes { refs, .. } = &mut mine.index {
                                refs[0].page = page;
                            }
                        }
                    }
                    _ => {
                        failed.push(format!("{case}: the tuple has no index to substitute"));
                        continue;
                    }
                }
                row
            }
        };
        directory.put(a, row).expect("a put");
        root.directory = directory.seal().expect("a directory seal");
        let (mut heap, cold) = reopened(&store, &root);
        let again = build(&mut heap)[0];
        let first = first_read(&mut heap, case, *read, [cold, again]);
        match first {
            Ok(Some(LoadError::Store(lash_kernel_doc::heap::HeapFault::Corrupt { .. }))) => {}
            Err(error) => failed.push(error),
            other => failed.push(format!("{case}: the first read answered {other:?}")),
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// The first load fault `read` of `cold` meets, its loads retried until
/// it completes; `None` when it completes with none, and an error naming
/// the case when a read fails other than by a load.
fn first_read(
    heap: &mut SlotHeap,
    case: &str,
    read: Read,
    held: [Value; 2],
) -> Result<Option<crate::heap::LoadError>, String> {
    let [cold, again] = held;
    let mut first = None;
    for _ in 0..64 {
        let asked = match read {
            Read::Item => heap
                .arena()
                .item(cold.handle().expect("a tuple"), 0)
                .map(|_| ()),
            Read::Equal => heap.arena().semantics().equal(&cold, &again).map(|_| ()),
            Read::Nested => heap
                .arena()
                .item(cold.handle().expect("a tuple"), 0)
                .and_then(|inner| {
                    heap.arena()
                        .item(inner.and_then(|inner| inner.handle()).expect("a tuple"), 0)
                })
                .map(|_| ()),
            Read::Copy => match crate::data::copy_out(heap.arena(), usize::MAX, &cold) {
                Ok(_) => Ok(()),
                Err(crate::data::CopyError::Cold { object, chunk }) => {
                    Err(lash_kernel_doc::arena::native::NativeError::Retry(
                        lash_kernel_doc::arena::native::Retry::Cold { object, chunk },
                    ))
                }
                Err(other) => return Err(format!("{case}: a copy failed: {other:?}")),
            },
        };
        match asked {
            Ok(()) => break,
            Err(lash_kernel_doc::arena::native::NativeError::Retry(
                lash_kernel_doc::arena::native::Retry::Cold { object, chunk },
            )) => {
                if let Err(error) = heap.load(object, chunk, &mut |visit| {
                    held.iter().for_each(&mut *visit)
                }) {
                    first = Some(error);
                    break;
                }
            }
            Err(other) => return Err(format!("{case}: a read failed: {other:?}")),
        }
    }
    Ok(first)
}

/// Seals the root tuple `roots[0]` (and, for [`Substitute::NestedRoot`],
/// the other tuple `roots[2]`), then rewrites the store as `substitute`
/// says: the head of the root tuple's first member keeps the member's key
/// and names the other tuple's root, the member's row then the other
/// tuple's; or one field of that head is forged, in the first chunk or
/// in the inline row, every other row kept.
fn nested(
    store: &Counting,
    heap: &mut SlotHeap,
    roots: &[Value; 3],
    substitute: Substitute,
) -> Result<lash_kernel_state::paged::run::root::HeapRoot, String> {
    use lash_kernel_state::kernel_internals::tree::{Access, grant};
    use lash_kernel_state::paged::PageKind;
    use lash_kernel_state::paged::run::chunked::{ChunkPage, ChunkRef, Placement};
    use lash_kernel_state::paged::run::directory::Directory;
    use lash_kernel_state::paged::run::index::{Index, Part};
    use lash_kernel_state::paged::run::objects::{Captures, Head, Record};
    use lash_kernel_value::tokens::{decode_body, encode_body};

    // The head forged as `forge` says, refused when a page cannot hold
    // the forgery (a page writes a tuple's root or its flat key, never
    // both, so the other is derived and a forgery of it is lost).
    let forged = |head: &mut Head, forge: fn(&mut Head) -> bool| -> Result<(), String> {
        let was = head.clone();
        if !forge(head) {
            return Err(format!("the member's head is another kind: {head:?}"));
        }
        let held: Head = decode_body(&encode_body(&*head)).map_err(|error| error.to_string())?;
        if held == was {
            return Err(format!("no page can hold the forgery: {head:?}"));
        }
        Ok(())
    };

    let sealed: &[Value] = match substitute {
        Substitute::NestedRoot => &[roots[0], roots[2]],
        _ => &roots[..1],
    };
    let mut root = heap
        .seal(sealed)
        .map_err(|error| format!("the seal failed: {error:?}"))?;
    let outer = root.roots[0].id().ok_or("the root is no object")?;
    let access = Access::new(Box::new(store.clone()));
    let mut directory = Directory::open(root.directory, Box::new(store.clone()));
    let mut row = directory
        .get(outer)
        .map_err(|error| format!("{error:?}"))?
        .ok_or("the root has no row")?;
    if let Substitute::InlineHead(forge) = substitute {
        // A closure holds the member in the cell it captures.
        let (holder, mut row) = match &row.record {
            Record::Chunked(_) => return Err("the holder is not inline".into()),
            Record::Closure {
                captures: Captures::Inline(cells),
                ..
            } => {
                let cell = *cells.first().ok_or("the closure captures no cell")?;
                let held = directory
                    .get(cell)
                    .map_err(|error| format!("{error:?}"))?
                    .ok_or("the cell has no row")?;
                (cell, held)
            }
            _ => (outer, row),
        };
        let (_, head) = row.heads.0.first_mut().ok_or("the row names no head")?;
        forged(head, forge)?;
        directory
            .put(holder, row)
            .map_err(|error| format!("{error:?}"))?;
        root.directory = directory.seal().map_err(|error| format!("{error:?}"))?;
        return Ok(root);
    }
    let replacement = match substitute {
        Substitute::NestedRoot => {
            let other = root.roots[1].id().ok_or("the other is no object")?;
            let Some(Head::Tuple {
                root: replacement, ..
            }) = root.heads.get(other).cloned()
            else {
                return Err("the other tuple has no head".into());
            };
            let theirs = directory
                .get(other)
                .map_err(|error| format!("{error:?}"))?
                .ok_or("the other has no row")?;
            Some((replacement, theirs))
        }
        _ => None,
    };
    let Record::Chunked(tuple) = &mut row.record else {
        return Err("the root tuple is not chunked".into());
    };
    let Index::Nodes { height: 1, refs } = &mut tuple.index else {
        return Err("the root tuple is not one index level deep".into());
    };
    let index_page = access
        .read(refs[0].page)
        .map_err(|error| format!("{error:?}"))?;
    let mut chunks: Vec<ChunkRef> =
        decode_body(&index_page.tokens).map_err(|error| error.to_string())?;
    for chunk in &mut chunks {
        grant(&mut chunk.page, &index_page.children).map_err(|error| format!("{error:?}"))?;
    }
    // A forged field is forged in every chunk of the first index page, so
    // a keyed read finds it whichever chunk its key is placed in; the
    // other tuple's root is named in the first chunk alone.
    let forged_chunks = match substitute {
        Substitute::NestedHead(_) => chunks.len(),
        _ => 1,
    };
    let mut member = None;
    for held in &mut chunks[..forged_chunks] {
        let chunk_page = access
            .read(held.page)
            .map_err(|error| format!("{error:?}"))?;
        let mut chunk: ChunkPage =
            decode_body(&chunk_page.tokens).map_err(|error| error.to_string())?;
        let (id, head) = chunk.heads.0.first_mut().ok_or("the chunk names no head")?;
        member = Some(*id);
        match (substitute, &replacement) {
            (Substitute::NestedRoot, Some((replacement, _))) => {
                let Head::Tuple { root: named, .. } = head else {
                    return Err("the member is no tuple".into());
                };
                *named = *replacement;
            }
            (Substitute::NestedHead(forge), None) => forged(head, forge)?,
            _ => return Err("not a nested substitution".into()),
        }
        held.page = access
            .seal(
                PageKind::SEQUENCE_LEAF,
                encode_body(&chunk),
                chunk_page.children.clone(),
            )
            .map_err(|error| format!("{error:?}"))?;
    }
    let member = member.ok_or("the index page names no chunk")?;
    let children = chunks.iter().map(|chunk| chunk.page).collect();
    let part: &mut Part<Option<Placement>> = &mut refs[0];
    part.page = access
        .seal(PageKind::SEQUENCE_INDEX, encode_body(&chunks), children)
        .map_err(|error| format!("{error:?}"))?;
    if let Some((_, theirs)) = replacement {
        directory
            .put(member, theirs)
            .map_err(|error| format!("{error:?}"))?;
    }
    directory
        .put(outer, row)
        .map_err(|error| format!("{error:?}"))?;
    root.directory = directory.seal().map_err(|error| format!("{error:?}"))?;
    Ok(root)
}
