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

use super::large::{Counting, fresh, make, reopened, warm_within};
use crate::heap::Heap;
use crate::heap::slots::SlotHeap;

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
    failed.extend(deep_rows());
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Members of each deep sequence: past what one level of index pages
/// below the row held (96 pages of 96 chunks of 64 members, 589,824), so
/// its index takes two levels below the row (cutover contract §6 index
/// rule).
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
        let mut root = heap.seal(&roots).expect("a seal");
        let target = |at: usize| root.roots[at].id().expect("an object");
        let (a, b) = (target(1), target(2));
        let mut directory = Directory::open(root.directory, Box::new(store.clone()));
        let other = directory.get(b).expect("a read").expect("a row");
        let row = match substitute {
            Substitute::Row => other,
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
        let held = [cold, again];
        let mut first = None;
        for _ in 0..64 {
            let asked = match read {
                Read::Item => heap
                    .arena()
                    .item(cold.handle().expect("a tuple"), 0)
                    .map(|_| ()),
                Read::Equal => heap.arena().semantics().equal(&cold, &again).map(|_| ()),
                Read::Copy => match crate::data::copy_out(heap.arena(), usize::MAX, &cold) {
                    Ok(_) => Ok(()),
                    Err(crate::data::CopyError::Cold { object, chunk }) => {
                        Err(lash_kernel_doc::arena::native::NativeError::Retry(
                            lash_kernel_doc::arena::native::Retry::Cold { object, chunk },
                        ))
                    }
                    Err(other) => panic!("{case}: a copy failed: {other:?}"),
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
                Err(other) => panic!("{case}: a read failed: {other:?}"),
            }
        }
        match first {
            Some(LoadError::Store(lash_kernel_doc::heap::HeapFault::Corrupt { .. })) => {}
            other => failed.push(format!("{case}: the first read answered {other:?}")),
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}
