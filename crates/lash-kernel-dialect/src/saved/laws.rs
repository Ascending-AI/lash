use super::*;
use lash_kernel_doc::graph_tokens::{ChildRef, decode_body, encode_body};
use lash_kernel_doc::heap_values::key_digest;
use lash_kernel_state::paged::{EditEpoch, ObjectInit, PageAnswer, PageBody, PageSource};
use std::cell::RefCell;
use std::rc::Rc;
#[derive(Default)]
struct Pages {
    rows: BTreeMap<PageId, PageAnswer>,
    reads: u64,
}
struct Source(Rc<RefCell<Pages>>);
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        let mut state = self.0.borrow_mut();
        state.reads += pages.len() as u64;
        pages
            .iter()
            .map(|p| {
                state
                    .rows
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect()
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
        heap.set_census(Box::new(LawCensus(heap.certificate())))
            .expect("census");
        (heap, pages)
    }
}
fn int(v: i64) -> Value {
    Value::Int(IntRef::Inline(v.into()))
}
fn id(value: &Value) -> EntityId {
    let Value::List(id) = value else {
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
        },
    );
    SavedFunction {
        name,
        document,
        captures: BTreeMap::from([(
            Name::new("capture"),
            heap.freeze(&Value::List(live), true).expect("freeze"),
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
    heap.list_write(live, 0, int(2)).expect("later live write");
    let function = loaded.member(0).expect("member");
    let first = function
        .captures_for_call(&mut heap)
        .expect("first invocation");
    let first_id = id(&first[&Name::new("capture")]);
    assert!(
        matches!(heap.list_get(first_id, 0).expect("original"), Some(Value::Int(IntRef::Inline(n))) if n == 1.into())
    );
    heap.list_write(first_id, 0, int(3))
        .expect("first invocation write");
    let second = function
        .captures_for_call(&mut heap)
        .expect("second invocation");
    let second_id = id(&second[&Name::new("capture")]);
    assert_ne!(first_id, second_id);
    assert!(
        matches!(heap.list_get(second_id, 0).expect("original again"), Some(Value::Int(IntRef::Inline(n))) if n == 1.into())
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
        .allocate_object(ObjectInit::List(vec![Value::Text(text)]))
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
        &FunctionValues::Bare,
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
            site: site.clone(),
            captures: vec![(Name::new("z"), z), (Name::new("a"), a)],
        })
        .expect("closure");
    let second = heap
        .allocate_object(ObjectInit::Closure {
            site,
            captures: vec![(Name::new("a"), a), (Name::new("z"), z)],
        })
        .expect("recompiled closure");
    let mut frozen = Vec::new();
    for id in [first, second] {
        let bindings = BTreeMap::from([(Name::new("read"), Value::Closure(id))]);
        let dropped = vec![Name::new("read")];
        let (saved, refused) = save(
            &mut heap,
            Left {
                document: &document,
                bindings: &bindings,
                not_carried: &dropped,
                controls: &BTreeMap::new(),
                annotations: None,
                values: &FunctionValues::Bare,
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
#[test]
fn a_bundle_is_its_functions_closure_and_is_refused_unless_its_members_agree() {
    let (mut heap, _) = fresh();
    let live = heap
        .allocate_object(ObjectInit::List(vec![]))
        .expect("list");
    let function = saved(&mut heap, live);
    let functions = BTreeMap::from([(function.name.clone(), function)]);
    let bundle = Bundle::of(&functions, &Name::new("read")).expect("bundle");
    let mut body = None;
    let mut document = None;
    bundle
        .stage::<HeapFault>(|kind, bytes| {
            if kind == DOCUMENT_NODE_KIND {
                document = Some(decode_body::<Document>(bytes).expect("doc"));
            } else {
                body = Some(decode_body::<BundleBody>(bytes).expect("body"));
            }
            Ok(ChildRef([kind as u8; 32]))
        })
        .expect("stage");
    let body = body.expect("body");
    let bytes = encode_body(&body);
    assert_eq!(
        encode_body(&decode_body::<BundleBody>(&bytes).expect("canonical")),
        bytes
    );
    let mut bad = document.expect("doc");
    bad.functions.clear();
    assert!(Bundle::assemble::<BundleError>(body, |_| Ok(bad.clone())).is_err());
}

struct LawCensus(lash_kernel_state::paged::AccountingCertificate);
impl lash_kernel_state::paged::AllocationCensus for LawCensus {
    fn restore(
        c: lash_kernel_state::paged::AccountingCertificate,
        _: LogicalBytes,
    ) -> Result<Self, lash_kernel_state::paged::CensusFault> {
        Ok(Self(c))
    }
    fn certificate(&self) -> lash_kernel_state::paged::AccountingCertificate {
        self.0
    }
    fn added(
        &mut self,
        cost: LogicalBytes,
        entities: u64,
    ) -> Result<(), lash_kernel_state::paged::CensusFault> {
        self.changed(LogicalBytes(0), cost)?;
        self.0.entities = self
            .0
            .entities
            .checked_add(entities)
            .expect("test entity count");
        Ok(())
    }
    fn changed(
        &mut self,
        before: LogicalBytes,
        after: LogicalBytes,
    ) -> Result<(), lash_kernel_state::paged::CensusFault> {
        self.0.upper.0 = self
            .0
            .upper
            .0
            .checked_sub(before.0)
            .and_then(|v| v.checked_add(after.0))
            .expect("test cost");
        Ok(())
    }
    fn reserve(&mut self, bytes: LogicalBytes) -> lash_kernel_state::paged::ReservationDecision {
        lash_kernel_state::paged::ReservationDecision::Granted(
            lash_kernel_state::paged::LogicalPermit { bytes },
        )
    }
    fn refine(
        &mut self,
        _: &mut dyn lash_kernel_state::paged::CensusGraph,
        _: &lash_kernel_state::paged::TransientRoots<'_>,
        _: lash_kernel_state::paged::PendingReservation,
    ) -> Result<lash_kernel_state::paged::RefinedDecision, lash_kernel_state::paged::CensusFault>
    {
        unreachable!("unbounded lifecycle law census")
    }
}
