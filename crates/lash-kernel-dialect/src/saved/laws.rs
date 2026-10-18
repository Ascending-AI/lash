use super::*;
use lash_kernel_doc::graph_tokens::{ChildRef, decode_body, encode_body};
use lash_kernel_doc::heap_values::key_digest;
use lash_kernel_state::kernel_internals::census_impl::Census;
use lash_kernel_state::paged::{
    AllocationCensus, EditEpoch, NameRef, ObjectInit, PageAnswer, PageBody, PageSource, SiteRef,
};
use std::cell::RefCell;
use std::rc::Rc;
#[derive(Default)]
struct Pages {
    rows: BTreeMap<PageId, PageAnswer>,
    reads: u64,
}
struct Source(Rc<RefCell<Pages>>);
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        let mut state = self.0.borrow_mut();
        state.reads += pages.len() as u64;
        Ok(pages
            .iter()
            .map(|p| {
                state
                    .rows
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend(&body.tokens);
        let page = PageRef {
            id: PageId(key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        self.0.borrow_mut().rows.insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}
fn fresh() -> (PagedHeap, Rc<RefCell<Pages>>) {
    let pages = Rc::new(RefCell::new(Pages::default()));
    {
        let mut heap = PagedHeap::fresh(Box::new(Source(Rc::clone(&pages))), EditEpoch(1));
        heap.set_census(Box::new(
            Census::restore(heap.certificate()).expect("census"),
        ))
        .expect("census");
        (heap, pages)
    }
}
fn int(v: i64) -> PageValue {
    PageValue::Int(IntRef::Inline(v.into()))
}
fn id(value: &PageValue) -> EntityId {
    let PageValue::List(id) = value else {
        panic!("list")
    };
    *id
}
fn saved(heap: &mut PagedHeap, live: EntityId) -> SavedFunction {
    let name = Name::new("read");
    let mut document = Document::new(lash_kernel_doc::NumberPolicy::Float, Vec::new());
    document.functions.insert(
        name.clone(),
        Function {
            params: Vec::new(),
            body: vec![Stmt::Return {
                value: Expr::Variable(Name::new("capture")),
            }],
            start: None,
        },
    );
    SavedFunction {
        name,
        document,
        captures: BTreeMap::from([(
            Name::new("capture"),
            heap.freeze(&PageValue::List(live), true).expect("freeze"),
        )]),
        release: PageRef {
            id: PageId([1; 32]),
            grant: PageGrant::NONE,
        },
        helper_owners: BTreeSet::new(),
        references: BTreeSet::new(),
        written: None,
        token: None,
    }
}
/// Writes the one value of a one-value list, as a projection writes its
/// dirty card.
fn write_first(
    heap: &mut lash_kernel_state::kernel_internals::PagedHeap,
    list: lash_kernel_doc::heap::EntityId,
    value: PageValue,
) -> Result<(), lash_kernel_state::paged::HeapWriteError> {
    heap.store_cards(
        list,
        lash_kernel_state::kernel_internals::StoredCards::List {
            len: 1,
            cards: vec![(0, vec![value])],
        },
    )
}

#[test]
fn a_saved_function_bundle_keeps_its_frozen_capture() {
    let (mut heap, _) = fresh();
    let live = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("list");
    let function = saved(&mut heap, live);
    let functions = BTreeMap::from([(function.name.clone(), function)]);
    let bundle = Bundle::of(&functions, &Name::new("read")).expect("bundle");
    let mut documents = BTreeMap::new();
    let mut body = None;
    let mut ordinal = 10u8;
    bundle
        .stage::<HeapFault>(|kind, bytes| {
            let reference = ChildRef([ordinal; 32]);
            ordinal += 1;
            if kind == DOCUMENT_NODE_KIND {
                documents.insert(reference, decode_body::<Document>(bytes).expect("document"));
            } else {
                body = Some(decode_body::<BundleBody>(bytes).expect("bundle body"));
            }
            Ok(reference)
        })
        .expect("stage");
    let loaded = Bundle::assemble::<BundleError>(body.expect("body"), |reference| {
        documents
            .get(&reference)
            .cloned()
            .ok_or(BundleError::Missing)
    })
    .expect("assemble");
    write_first(&mut heap, live, int(2)).expect("later live write");
    let function = loaded.member(0).expect("member");
    let first = function
        .captures_for_call(&mut heap)
        .expect("first invocation");
    let first_id = id(&first[&Name::new("capture")]);
    assert!(
        matches!(heap.list_get(first_id, 0).expect("original"), Some(PageValue::Int(IntRef::Inline(n))) if n == 1.into())
    );
    write_first(&mut heap, first_id, int(3)).expect("first invocation write");
    let second = function
        .captures_for_call(&mut heap)
        .expect("second invocation");
    let second_id = id(&second[&Name::new("capture")]);
    assert_ne!(first_id, second_id);
    assert!(
        matches!(heap.list_get(second_id, 0).expect("original again"), Some(PageValue::Int(IntRef::Inline(n))) if n == 1.into())
    );
}
#[derive(Debug)]
enum BundleError {
    Refused,
    Missing,
}
impl From<BundleRefusal> for BundleError {
    fn from(_: BundleRefusal) -> Self {
        Self::Refused
    }
}
#[test]
fn installing_loads_no_capture_payload() {
    let (mut heap, pages) = fresh();
    let text = heap.text(&"x".repeat(1_000_000)).expect("text");
    let live = heap
        .allocate_object(ObjectInit::List(vec![PageValue::Text(text)]))
        .expect("list");
    let function = saved(&mut heap, live);
    let functions = BTreeMap::from([(function.name.clone(), function)]);
    let mut document = Document::new(lash_kernel_doc::NumberPolicy::Float, Vec::new());
    heap.evict().expect("evict");
    let reads = pages.borrow().reads;
    let declarations = install(
        &mut document,
        &functions,
        &BTreeSet::from([Name::new("read")]),
        &lash_kernel_doc::FunctionRepresentation::Bare,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &|_| true,
    )
    .expect("install");
    assert_eq!(declarations.len(), 1);
    assert_eq!(pages.borrow().reads, reads);
    assert!(matches!(
        document.functions[&Name::new("read")].body[0],
        Stmt::Return { .. }
    ));
}
#[test]
fn saved_captures_use_names_when_free_variable_slots_are_reordered() {
    let (mut heap, _) = fresh();
    let a = heap
        .allocate_object(ObjectInit::Variable(int(1)))
        .expect("a");
    let z = heap
        .allocate_object(ObjectInit::Variable(int(2)))
        .expect("z");
    let closure = lash_kernel_doc::Closure {
        params: Vec::new(),
        body: vec![Stmt::Return {
            value: Expr::Tuple(vec![
                Expr::Variable(Name::new("z")),
                Expr::Variable(Name::new("a")),
            ]),
        }],
        start: None,
    };
    let document = Document::new(
        lash_kernel_doc::NumberPolicy::Float,
        vec![Stmt::Let {
            name: Name::new("read"),
            value: Rhs::Expr(Expr::Closure(Box::new(closure))),
        }],
    );
    let site = Site {
        unit: Unit::Main,
        path: vec![0, 0],
    };
    let first = heap
        .allocate_object(ObjectInit::Closure {
            site: SiteRef::new(&site, heap.pages()).unwrap(),
            captures: vec![
                (NameRef::new(&Name::new("z"), heap.pages()).unwrap(), z),
                (NameRef::new(&Name::new("a"), heap.pages()).unwrap(), a),
            ],
        })
        .expect("closure");
    let second = heap
        .allocate_object(ObjectInit::Closure {
            site: SiteRef::new(&site, heap.pages()).unwrap(),
            captures: vec![
                (NameRef::new(&Name::new("a"), heap.pages()).unwrap(), a),
                (NameRef::new(&Name::new("z"), heap.pages()).unwrap(), z),
            ],
        })
        .expect("recompiled closure");
    let mut frozen = Vec::new();
    for id in [first, second] {
        let bindings = BTreeMap::from([(Name::new("read"), PageValue::Closure(id))]);
        let dropped = vec![Name::new("read")];
        let (saved, refused) = save(
            &mut heap,
            Left {
                document: &document,
                bindings: &bindings,
                not_carried: &dropped,
                controls: &BTreeMap::new(),
                written: &BTreeMap::new(),
                values: &lash_kernel_doc::FunctionRepresentation::Bare,
                release: PageRef {
                    id: PageId([1; 32]),
                    grant: PageGrant::NONE,
                },
            },
            &BTreeSet::new(),
        )
        .expect("save");
        assert!(refused.is_empty(), "{refused:?}");
        let captures = &saved[&Name::new("read")].function.captures;
        frozen.push(
            captures
                .iter()
                .map(|(name, value)| (name.clone(), value.root.id))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(frozen[0], frozen[1]);
}
/// A saved function `name` whose body calls each of `calls` by name, and
/// whose forwarded private bodies `owners` declare.
fn calling(name: &str, calls: &[&str], owners: &[&str]) -> SavedFunction {
    let mut document = Document::new(lash_kernel_doc::NumberPolicy::Float, Vec::new());
    document.functions.insert(
        Name::new(name),
        Function {
            params: Vec::new(),
            body: calls
                .iter()
                .map(|callee| Stmt::Let {
                    name: Name::new(format!("{callee}_called")),
                    value: Rhs::Action(Action::Call {
                        callee: Callee::Declared(Name::new(*callee)),
                        args: Vec::new(),
                    }),
                })
                .collect(),
            start: None,
        },
    );
    SavedFunction {
        name: Name::new(name),
        document,
        captures: BTreeMap::new(),
        release: PageRef {
            id: PageId([1; 32]),
            grant: PageGrant::NONE,
        },
        helper_owners: owners.iter().map(|owner| Name::new(*owner)).collect(),
        references: BTreeSet::new(),
        written: None,
        token: None,
    }
}
/// The body a bundle stages, and the documents it staged by reference.
fn staged(bundle: &Bundle) -> (BundleBody, BTreeMap<ChildRef, Document>) {
    let mut documents = BTreeMap::new();
    let mut body = None;
    let mut next = 0_u8;
    bundle
        .stage::<HeapFault>(|kind, bytes| {
            next += 1;
            let reference = ChildRef([next; 32]);
            if kind == DOCUMENT_NODE_KIND {
                documents.insert(reference, decode_body::<Document>(bytes).expect("doc"));
            } else {
                assert_eq!(kind, FUNCTION_BUNDLE_NODE_KIND);
                body = Some(decode_body::<BundleBody>(bytes).expect("body"));
            }
            Ok(reference)
        })
        .expect("stage");
    (body.expect("body"), documents)
}
/// A bundle holds its function and every saved function it uses, each once:
/// two functions that use one another make one bundle whose members are
/// both, and a function nothing it uses names stays out. A forwarded private
/// body's named owner is used as a call is (FIG-6137): mutually recursive
/// exports whose calls go through private positional bodies still make one
/// bundle. Read back, the bundle is the one staged, and a body whose
/// required effects are not its members' own, or whose member its document
/// does not declare, is refused.
#[test]
fn a_bundle_is_its_functions_closure_and_is_refused_unless_its_members_agree() {
    let functions: BTreeMap<Name, SavedFunction> = [
        calling("even", &["odd"], &[]),
        calling("odd", &["even"], &[]),
        calling("apart", &[], &[]),
        calling("forwarded_even", &[], &["forwarded_odd"]),
        calling("forwarded_odd", &[], &["forwarded_even"]),
    ]
    .into_iter()
    .map(|function| (function.name.clone(), function))
    .collect();
    let names = |bundle: &Bundle| {
        bundle
            .members()
            .keys()
            .map(|name| name.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    let even = Bundle::of(&functions, &Name::new("even")).expect("a held function");
    let odd = Bundle::of(&functions, &Name::new("odd")).expect("a held function");
    assert_eq!(names(&even), ["even", "odd"]);
    assert_eq!(
        names(&odd),
        names(&even),
        "one member table for the mutual pair"
    );
    assert_eq!(even.index(&Name::new("odd")), Some(1));
    assert_eq!(
        names(&Bundle::of(&functions, &Name::new("apart")).unwrap()),
        ["apart"]
    );
    for root in ["forwarded_even", "forwarded_odd"] {
        assert_eq!(
            names(&Bundle::of(&functions, &Name::new(root)).unwrap()),
            ["forwarded_even", "forwarded_odd"],
            "{root}: the owner edge makes the forwarded pair one bundle"
        );
    }

    let (body, documents) = staged(&even);
    let (again, _) = staged(&odd);
    assert_eq!(encode_body(&body), encode_body(&again));
    let bytes = encode_body(&body);
    assert_eq!(
        encode_body(&decode_body::<BundleBody>(&bytes).expect("canonical")),
        bytes
    );
    let read = |body: BundleBody| {
        Bundle::assemble::<BundleRefusal>(body, |reference| Ok(documents[&reference].clone()))
    };
    let read_back = read(body.clone()).expect("the staged bundle");
    assert_eq!(names(&read_back), names(&even));
    assert_eq!(encode_body(&staged(&read_back).0), bytes);

    let mut effects = body.clone();
    effects.effects.push((
        lash_kernel_doc::EffectName::new("echo").expect("an effect name"),
        lash_kernel_doc::Signature {
            params: Vec::new(),
            result: lash_kernel_doc::Type::Any,
        },
    ));
    assert!(matches!(read(effects), Err(BundleRefusal::RequiredEffects)));
    let mut renamed = body;
    renamed.members[0].name = Name::new("evens");
    assert!(matches!(
        read(renamed),
        Err(BundleRefusal::Undeclared { member }) if member == Name::new("evens")
    ));
    // A capture's document is read back with the bundle.
    let (mut heap, _) = fresh();
    let live = heap
        .allocate_object(ObjectInit::List(vec![]))
        .expect("list");
    let function = saved(&mut heap, live);
    let functions = BTreeMap::from([(function.name.clone(), function)]);
    let bundle = Bundle::of(&functions, &Name::new("read")).expect("bundle");
    let (body, documents) = staged(&bundle);
    let mut bad = documents.values().next().expect("doc").clone();
    bad.functions.clear();
    assert!(Bundle::assemble::<BundleError>(body, |_| Ok(bad.clone())).is_err());
}
