//! The token grammar law for the run heap's page and record encoders: what
//! each encoder writes is exactly one token tree, each sequence and sum
//! declaring as many fields as follow it. The walk knows only the grammar
//! (`TokenReader::skip_tree`), never the type, so a count an encoder and
//! its mirrored decoder agree on but the grammar does not is still caught.
use super::super::run::chunked::{
    ChunkPage, ChunkRef, Chunked, EdgePage, EdgePart, Entry, Members, Placement,
};
use super::super::run::directory::{DirectoryRoot, Row};
use super::super::run::ids::{HighWater, ObjectId};
use super::super::run::index::{Index, Part};
use super::super::run::objects::{
    Captures, ClosureCode, Code, Head, Heads, ObjectKind, Paged, Record, RunValue, ScalarPayload,
};
use super::super::run::root::HeapRoot;
use super::super::{NameRef, SiteRef, TaskIdentityRef};
use super::pages::fresh;
use crate::arena::Summary;
use lash_kernel_doc::heap::{
    IntSummary, NormalKey, PageGrant, PageId, PageRef, PayloadRoot, TextSummary,
};
use lash_kernel_doc::{Float, Handle, Integer};
use lash_kernel_value::Name;
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::{CallableId, FunctionId};
use lash_kernel_value::sites::{Site, Unit};
use lash_kernel_value::tokens::{TokenCodec, TokenReader, encode_body};

fn page(byte: u8) -> PageRef {
    PageRef {
        id: PageId([byte; 32]),
        grant: PageGrant([byte; 48]),
    }
}

fn payload(byte: u8) -> PayloadRoot {
    PayloadRoot {
        page: page(byte),
        id: [byte; 32],
    }
}

fn text(bytes: u64) -> TextSummary {
    TextSummary {
        utf8_bytes: bytes,
        chars: bytes,
        utf16_units: bytes,
    }
}

fn callable() -> CallableId {
    CallableId {
        link: LinkId::from_bytes([1; 32]),
        function: FunctionId::from_bytes([2; 32]),
    }
}

fn shape() -> Summary {
    Summary {
        depth: 2,
        task_edges: 1,
        has_edges: true,
    }
}

fn int() -> IntSummary {
    IntSummary {
        bits: 70,
        negative: true,
    }
}

fn closure_code() -> ClosureCode {
    ClosureCode {
        capture_names: vec![Name::new("a"), Name::new("b")],
        callable: callable(),
        link: LinkId::from_bytes([1; 32]),
        site: Site::new(Unit::Main, [3, 4]),
    }
}

/// Every variant of a run value.
fn values() -> Vec<RunValue> {
    vec![
        RunValue::Null,
        RunValue::Absent,
        RunValue::Bool(true),
        RunValue::Int(-7),
        RunValue::Float(Float::new(1.5)),
        RunValue::Text("short".into()),
        RunValue::Timestamp(9),
        RunValue::Task(3),
        RunValue::TaskRef(4),
        RunValue::Object(ObjectKind::Tuple, ObjectId(5)),
        RunValue::Ref(ObjectId(6)),
    ]
}

/// Every variant of a head, each optional form both ways.
fn heads() -> Vec<Head> {
    let mut heads = Vec::new();
    for normal in [
        None,
        Some(NormalKey::Digest([7; 32])),
        Some(NormalKey::Inline(vec![0x09, 0x00])),
    ] {
        heads.push(Head::Tuple {
            len: 3,
            shape: shape(),
            normal,
            root: [8; 32],
        });
    }
    let normals = [NormalKey::Digest([9; 32]), NormalKey::Inline(vec![1, 2, 3])];
    heads.push(Head::Error {
        shape: shape(),
        content: [10; 32],
    });
    for normal in normals {
        heads.push(Head::BigInt {
            summary: int(),
            normal: normal.clone(),
        });
        heads.push(Head::Timestamp {
            summary: int(),
            normal: normal.clone(),
        });
        heads.push(Head::LongText {
            summary: text(40),
            normal: normal.clone(),
        });
        heads.push(Head::Bytes {
            len: 40,
            normal: normal.clone(),
        });
        heads.push(Head::Handle {
            kind_len: 2,
            id_len: 3,
            content: normal,
        });
    }
    heads.push(Head::Function(callable()));
    heads
}

fn chunk_refs() -> Vec<ChunkRef> {
    let mut refs = Vec::new();
    for first in [None, Some([11; 32])] {
        for leaf in [None, Some([12; 32])] {
            refs.push(ChunkRef {
                first,
                len: 64,
                page: page(13),
                leaf,
            });
        }
    }
    refs
}

fn edge_part() -> EdgePart {
    EdgePart {
        first: ObjectId(14),
        ids: 2,
        page: page(15),
    }
}

/// Both forms of every index the heap holds, each node form under each
/// key form with and without a content id.
fn chunked() -> Vec<Chunked> {
    let placed: Vec<Part<Option<Placement>>> = [None, Some([16; 32])]
        .into_iter()
        .flat_map(|first| {
            [None, Some([17; 32])].map(|id| Part {
                first,
                count: 2,
                page: page(18),
                id,
            })
        })
        .collect();
    let edges: Vec<Part<ObjectId>> = [None, Some([19; 32])]
        .map(|id| Part {
            first: ObjectId(20),
            count: 2,
            page: page(21),
            id,
        })
        .to_vec();
    vec![
        Chunked {
            kind: ObjectKind::List,
            len: 300,
            next: 300,
            index: Index::Leaves(chunk_refs()),
            edges: Index::Leaves(vec![edge_part()]),
        },
        Chunked {
            kind: ObjectKind::Map,
            len: 300,
            next: 301,
            index: Index::Nodes {
                height: 1,
                refs: placed,
            },
            edges: Index::Nodes {
                height: 1,
                refs: edges,
            },
        },
    ]
}

/// Every variant of a record, each inner form both ways.
fn records() -> Vec<Record> {
    let pair = (RunValue::Text("k".into()), RunValue::Int(1));
    let mut records = vec![
        Record::List(values()),
        Record::Tuple(values()),
        Record::Set(values()),
        Record::Map(vec![pair.clone()]),
        Record::Record(vec![pair]),
        Record::Cell(RunValue::Int(2)),
        Record::Error {
            kind: RunValue::Text("E".into()),
            message: RunValue::Text("m".into()),
            data: RunValue::Null,
        },
        Record::BigInt(Integer::from(-3)),
        Record::Timestamp(Integer::from(4)),
        Record::LongText("long".into()),
        Record::Bytes(vec![1, 2, 3].into()),
        Record::Function(callable()),
        Record::Handle(Handle {
            kind: "file".into(),
            id: "1".into(),
        }),
    ];
    for is_text in [true, false] {
        records.push(Record::Paged(Paged {
            text: is_text,
            summary: text(70_000),
            root: payload(22),
        }));
    }
    for kind in [
        ObjectKind::BigInt,
        ObjectKind::Timestamp,
        ObjectKind::Handle,
    ] {
        records.push(Record::Scalar(ScalarPayload {
            kind,
            summary: text(300),
            root: payload(23),
        }));
    }
    for chunked in chunked() {
        records.push(Record::Chunked(Box::new(chunked)));
    }
    for code in [
        Code::Inline(closure_code()),
        Code::Paged {
            bytes: 70_000,
            root: payload(24),
        },
    ] {
        for captures in [
            Captures::Inline(vec![ObjectId(25), ObjectId(26)]),
            Captures::Paged(Box::new(chunked().remove(0))),
        ] {
            records.push(Record::Closure {
                code: code.clone(),
                captures,
            });
        }
    }
    records
}

fn walk(failures: &mut Vec<String>, name: &str, tokens: &[u8]) {
    let mut reader = TokenReader::new(tokens);
    let walked = reader
        .skip_tree()
        .map(|_| ())
        .and_then(|()| reader.finish());
    if let Err(error) = walked {
        failures.push(format!("{name}: {error}"));
    }
}

fn each<T: TokenCodec + std::fmt::Debug>(failures: &mut Vec<String>, items: &[T]) {
    for item in items {
        walk(failures, &format!("{item:?}"), &encode_body(item));
    }
}

/// Each page and record encoder of the run heap, over a representative of
/// every variant and arity, writes one whole token tree.
#[test]
fn every_heap_encoder_writes_one_token_tree() {
    let mut failures = Vec::new();
    let kinds = [
        ObjectKind::List,
        ObjectKind::Map,
        ObjectKind::Set,
        ObjectKind::Record,
        ObjectKind::Closure,
        ObjectKind::Tuple,
        ObjectKind::Error,
        ObjectKind::BigInt,
        ObjectKind::Timestamp,
        ObjectKind::LongText,
        ObjectKind::Bytes,
        ObjectKind::Function,
        ObjectKind::Handle,
        ObjectKind::Cell,
    ];
    each(&mut failures, &kinds);
    each(&mut failures, &values());
    each(&mut failures, &heads());
    each(
        &mut failures,
        &[Heads(vec![]), Heads(vec![(ObjectId(1), heads().remove(0))])],
    );
    each(&mut failures, &[ObjectId(0), ObjectId(u64::MAX)]);
    each(&mut failures, &[HighWater(0), HighWater(9)]);
    each(&mut failures, &chunk_refs());
    each(&mut failures, &[edge_part()]);
    each(
        &mut failures,
        &[EdgePage(vec![]), EdgePage(vec![(ObjectId(1), 2)])],
    );
    for chunked in chunked() {
        walk(&mut failures, "chunk index", &encode_body(&chunked.index));
        walk(&mut failures, "edge index", &encode_body(&chunked.edges));
        if let Index::Nodes { refs, .. } = &chunked.index {
            each(&mut failures, refs);
        }
        if let Index::Nodes { refs, .. } = &chunked.edges {
            each(&mut failures, refs);
        }
    }
    let unit: Vec<Part<()>> = [None, Some([27; 32])]
        .map(|id| Part {
            first: (),
            count: 1,
            page: page(28),
            id,
        })
        .to_vec();
    each(&mut failures, &unit);
    each(&mut failures, &chunked());
    each(
        &mut failures,
        &[
            ChunkPage {
                members: Members::List(values()),
                heads: Heads(vec![(ObjectId(5), heads().remove(0))]),
            },
            ChunkPage {
                members: Members::Table(vec![Entry {
                    seq: 1,
                    placement: [29; 32],
                    key: RunValue::Text("k".into()),
                    value: RunValue::Int(1),
                }]),
                heads: Heads::default(),
            },
        ],
    );
    each(&mut failures, &[closure_code()]);
    each(
        &mut failures,
        &[
            Code::Inline(closure_code()),
            Code::Paged {
                bytes: 70_000,
                root: payload(30),
            },
        ],
    );
    each(
        &mut failures,
        &[
            Captures::Inline(vec![ObjectId(1)]),
            Captures::Paged(Box::new(chunked().remove(0))),
        ],
    );
    let records = records();
    each(&mut failures, &records);
    let rows: Vec<Row> = records
        .into_iter()
        .map(|record| Row::of(record, Heads::default()))
        .collect();
    each(&mut failures, &rows);
    let directories = [
        DirectoryRoot {
            page: None,
            rows: 0,
        },
        DirectoryRoot {
            page: Some(page(31)),
            rows: 9,
        },
    ];
    each(&mut failures, &directories);
    each(
        &mut failures,
        &[HeapRoot {
            high_water: HighWater(10),
            directory: directories[1],
            roots: values(),
            heads: Heads(vec![(ObjectId(5), heads().remove(0))]),
        }],
    );
    let (access, _) = fresh();
    let short = Name::new("n");
    let long = Name::new("n".repeat(70_000));
    each(
        &mut failures,
        &[
            NameRef::new(&short, &access).unwrap(),
            NameRef::new(&long, &access).unwrap(),
        ],
    );
    let site = |name: &Name| Site::new(Unit::Function(name.clone()), [1, 2]);
    let sites = [
        SiteRef::new(&Site::new(Unit::Main, [0]), &access).unwrap(),
        SiteRef::new(&site(&short), &access).unwrap(),
        SiteRef::new(&site(&long), &access).unwrap(),
    ];
    each(&mut failures, &sites);
    let spawned =
        TaskIdentityRef::spawn(TaskIdentityRef::MAIN, sites[1].clone(), 3, &access).unwrap();
    each(&mut failures, &[TaskIdentityRef::MAIN, spawned]);
    assert!(
        failures.is_empty(),
        "encoders whose output is not one token tree:\n{}",
        failures.join("\n")
    );
}
