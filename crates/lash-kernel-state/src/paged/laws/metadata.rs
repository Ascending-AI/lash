//! Bounded S1/S4 metadata and exact diagnostic projections.
use super::super::directory::ordinal_key;
use super::super::tree::{SealWork, Tree};
use super::super::*;
use super::heap::{fresh, seal};
use lash_kernel_doc::graph_tokens::{decode_body, encode_body};
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{Datum, Name, Site, TaskIdentity, Unit};

fn work() -> SealWork {
    SealWork::new(SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    })
}

/// S4 metadata amendment: a long binding and capture name are descriptors,
/// and sealing their rows never materializes or copies the payload again.
#[test]
fn megabyte_binding_and_capture_names_keep_bounded_cells() {
    let (mut heap, pages) = fresh();
    let name = Name::new("x".repeat(1 << 20));
    let named = NameRef::new(&name, &heap.access).unwrap();
    let site = SiteRef::new(&Site::new(Unit::Function(name.clone()), [0]), &heap.access).unwrap();
    let variable = heap
        .allocate_object(ObjectInit::Variable(Value::Null))
        .unwrap();
    let closure = heap
        .allocate_object(ObjectInit::Closure {
            site: site.clone(),
            captures: vec![(named.clone(), variable)],
        })
        .unwrap();
    let key = metadata::metadata_key(&name);
    let mut bindings = Tree::new(false);
    bindings
        .write(
            key.clone(),
            BindingRecord {
                name: named,
                declared: site,
                value: BoundRecord::Value(Value::Closure(closure)),
            },
            &heap.access,
        )
        .unwrap();
    let before = pages.borrow().seals;
    bindings.seal(&heap.access, &mut work()).unwrap();
    seal(&mut heap);
    assert!(
        pages.borrow().seals - before <= 4,
        "bounded rows, directory and header"
    );
    let root = bindings.page().unwrap();
    let restored = Tree::<BindingRecord>::open(
        Some(root),
        bindings.len(),
        LogicalBytes(bindings.cost()),
        false,
    );
    let binding = restored.get(&key, &heap.access).unwrap().unwrap();
    assert_eq!(binding.name.resolve(&heap.access).unwrap(), name);
    assert_eq!(
        binding.declared.resolve(&heap.access).unwrap().unit,
        Unit::Function(name.clone())
    );
    let closure = heap.closure(closure).unwrap();
    let capture = closure.captures.get(&key, &heap.access).unwrap().unwrap();
    assert_eq!(capture.0.resolve(&heap.access).unwrap(), name);
    assert_eq!(capture.1, variable);
    assert!(
        pages
            .borrow()
            .pages
            .values()
            .all(|page| page.tokens.len() + page.children.len() * 32 + 32 <= 65_536)
    );
}

/// S4 ancestry amendment: each serial spawn writes one shared parent node,
/// and its task row costs the same bounded work at every lineage depth.
#[test]
fn ten_thousand_spawn_ancestors_have_bounded_writes_and_exact_diagnostics() {
    let (mut heap, pages) = fresh();
    let site = Site::new(Unit::Main, [7]);
    let saved_site = SiteRef::new(&site, &heap.access).unwrap();
    let mut identity = TaskIdentityRef::MAIN;
    let mut tasks = Tree::new(false);
    for occurrence in 0..10_000 {
        let before = pages.borrow().seals;
        identity =
            TaskIdentityRef::spawn(identity, saved_site.clone(), occurrence, &heap.access).unwrap();
        tasks
            .write(
                ordinal_key(0),
                TaskRecord {
                    identity,
                    state: TaskStateRecord::Ready,
                    result_retained: true,
                    joiners: Tree::new(true),
                    observed: false,
                    passed: false,
                    failed: false,
                    occurrences: Tree::new(false),
                    frames: Tree::new(true),
                },
                &heap.access,
            )
            .unwrap();
        heap.set_execution_roots(tasks.clone(), Tree::new(false), &[], &[], &[])
            .unwrap();
        tasks.seal(&heap.access, &mut work()).unwrap();
        assert_eq!(
            pages.borrow().seals - before,
            2,
            "one ancestry node and one task row"
        );
        assert!(pages.borrow().pages[&tasks.page().unwrap().id].tokens.len() < 512);
    }
    let mut projected = identity.resolve(&heap.access).unwrap();
    for occurrence in (0..10_000).rev() {
        let TaskIdentity::Spawned(spawned) = projected else {
            panic!("missing ancestor");
        };
        assert_eq!(spawned.site, site);
        assert_eq!(spawned.occurrence, occurrence);
        // Consume iteratively so the diagnostic law does not depend on
        // the host test thread's recursive Arc destructor stack.
        projected = match std::sync::Arc::try_unwrap(spawned.parent) {
            Ok(parent) => parent,
            Err(_) => panic!("projection owns its parent"),
        };
    }
    assert!(matches!(projected, TaskIdentity::Main));
}

/// S1 function-name amendment / D-KEY: payload placement cannot change a
/// function's name, normal form or boundary representation.
#[test]
fn a_megabyte_function_name_is_bounded_and_round_trips_exactly() {
    let (mut heap, pages) = fresh();
    let name = Name::new("f".repeat(1 << 20));
    let descriptor = heap.text(name.as_str()).unwrap();
    let value = Value::Function(descriptor.clone());
    let resident = Value::Function(TextRef {
        normal: Default::default(),
        summary: descriptor.summary,
        body: Payload::Resident(std::sync::Arc::from(name.as_str())),
    });
    assert!(heap.equal(&value, &resident).unwrap());
    assert_eq!(
        heap.normal_key(&value).unwrap(),
        heap.normal_key(&resident).unwrap()
    );
    assert_ne!(
        heap.normal_key(&value).unwrap(),
        heap.normal_key(&Value::Text(descriptor)).unwrap(),
        "function and text key domains differ"
    );
    let tuple = heap.tuple(vec![value.clone()]).unwrap();
    let mut bindings = Tree::new(false);
    bindings
        .write(
            ordinal_key(0),
            BindingRecord {
                name: NameRef::new(&Name::from("function"), &heap.access).unwrap(),
                declared: SiteRef::new(&Site::new(Unit::Main, [0]), &heap.access).unwrap(),
                value: BoundRecord::Value(value.clone()),
            },
            &heap.access,
        )
        .unwrap();
    bindings.seal(&heap.access, &mut work()).unwrap();
    let member = heap.tuple_get(&tuple, 0).unwrap().unwrap();
    assert!(encode_body(&member).len() < 256);
    let frozen = heap.freeze(&value, true).unwrap();
    let datum = heap.frozen_datum(&frozen).unwrap();
    assert_eq!(datum, Datum::Function(name));
    let restored = heap.freeze_datum(&datum).unwrap();
    let value2 = heap.instantiate(&restored).unwrap();
    assert!(heap.equal(&value, &value2).unwrap());
    let decoded: Value = decode_body(&encode_body(&member)).unwrap();
    assert!(matches!(
        decoded,
        Value::Function(TextRef {
            body: Payload::Paged(_),
            ..
        })
    ));
    assert!(
        pages
            .borrow()
            .pages
            .values()
            .all(|page| page.tokens.len() + page.children.len() * 32 + 32 <= 65_536)
    );
}

/// D-CAP/S4: admitted metadata names share descriptors, and diagnostic
/// projection shares equal payloads without retaining an unbounded cache.
#[test]
fn repeated_long_metadata_names_share_storage_and_diagnostic_text() {
    let (heap, pages) = fresh();
    let name = Name::new("n".repeat(1 << 20));
    let first = NameRef::new(&name, &heap.access).unwrap();
    let before = pages.borrow().seals;
    for _ in 0..1000 {
        let site =
            SiteRef::new(&Site::new(Unit::Function(name.clone()), [0]), &heap.access).unwrap();
        assert!(
            site.matches(&Site::new(Unit::Function(name.clone()), [0]), &heap.access)
                .unwrap()
        );
        assert!(first.same_version(&NameRef::new(&name, &heap.access).unwrap()));
        assert!(heap.access.clean_bytes() <= 64 << 20);
    }
    assert_eq!(pages.borrow().seals, before);
    let a = first.resolve(&heap.access).unwrap();
    let b = first.resolve(&heap.access).unwrap();
    assert_eq!(
        a.as_str().as_ptr(),
        b.as_str().as_ptr(),
        "one diagnostic text allocation"
    );
    assert_eq!(a, name);
}

/// S1 fault law: a decoded-name cache must retain the descriptor's summary
/// validation, even when the referenced payload was previously read.
#[test]
fn a_cached_metadata_name_does_not_hide_a_bad_payload_summary() {
    let (heap, _) = fresh();
    let first = NameRef::new(&Name::new("n".repeat(1 << 20)), &heap.access).unwrap();
    first.resolve(&heap.access).unwrap();
    let mut text: TextRef = decode_body(&encode_body(&first)).unwrap();
    text.summary.utf8_bytes += 1;
    text.summary.utf16_units += 1;
    text.summary.chars += 1;
    let bad: NameRef = decode_body(&encode_body(&text)).unwrap();
    assert!(matches!(
        bad.resolve(&heap.access),
        Err(HeapFault::Corrupt { .. })
    ));
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CountedRow(u64);
thread_local! {
    static ROW_ENCODINGS: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}
impl lash_kernel_doc::graph_tokens::TokenCodec for CountedRow {
    fn encode(&self, out: &mut lash_kernel_doc::graph_tokens::TokenWriter) {
        ROW_ENCODINGS.with_borrow_mut(|rows| rows.push(self.0));
        self.0.encode(out);
    }
    fn decode(
        input: &mut lash_kernel_doc::graph_tokens::TokenReader<'_>,
    ) -> Result<Self, lash_kernel_doc::graph_tokens::TokenError> {
        Ok(Self(u64::decode(input)?))
    }
}
impl super::super::tree::Cell for CountedRow {
    fn cost(&self) -> u64 {
        self.0 + 1
    }
    fn task_edges(&self) -> u64 {
        self.0 % 2
    }
    fn live_members(&self) -> u64 {
        1
    }
}

/// FIG-6566: a missing binding changes no version, residency or page,
/// including a cold sealed/inline leaf and a multi-level search path.
#[test]
fn an_absent_removal_dirties_and_encodes_nothing() {
    use lash_kernel_doc::graph_tokens::TokenCodec;
    for count in [8, 128] {
        for form in ["dirty", "sealed", "inline"] {
            if form == "inline" && count > 8 {
                continue;
            }
            let (heap, pages) = fresh();
            let mut tree = Tree::new(false);
            for i in 0..count {
                tree.write(ordinal_key(i * 2), CountedRow(i), &heap.access)
                    .unwrap();
            }
            if form == "sealed" {
                tree.seal(&heap.access, &mut work()).unwrap();
            } else if form == "inline" {
                tree.seal_nested(&heap.access, &mut work()).unwrap();
                tree = Tree::decode(&mut lash_kernel_doc::graph_tokens::TokenReader::new(
                    &encode_body(&tree),
                ))
                .unwrap();
            }
            heap.access.clear();
            let prior = tree.clone();
            let bytes = heap.access.dirty_bytes();
            let seals = pages.borrow().seals;
            ROW_ENCODINGS.with_borrow_mut(Vec::clear);
            super::super::tree::take_key_copies();
            assert_eq!(tree.remove(&ordinal_key(3), &heap.access).unwrap(), None);
            let encodes = ROW_ENCODINGS.with_borrow(|rows| rows.len());
            assert_eq!(encodes, 0, "{form}, {count} rows");
            assert!(tree.same_version(&prior), "{form}, {count} rows");
            assert_eq!(heap.access.dirty_bytes(), bytes);
            if form == "dirty" {
                assert_eq!(super::super::tree::take_key_copies(), 0);
            }
            assert_eq!(pages.borrow().seals, seals);
        }
    }
}

/// FIG-6566: removing one resident row measures that row only; its
/// neighbours and retained working version keep their contents and sums.
#[test]
fn removing_one_row_encodes_only_the_removed_row() {
    for count in [8, 128] {
        let (heap, _) = fresh();
        let mut tree = Tree::new(false);
        for i in 0..count {
            tree.write(ordinal_key(i), CountedRow(i), &heap.access)
                .unwrap();
        }
        let prior = tree.clone();
        let removed = count / 2;
        ROW_ENCODINGS.with_borrow_mut(Vec::clear);
        assert_eq!(
            tree.remove(&ordinal_key(removed), &heap.access).unwrap(),
            Some(CountedRow(removed))
        );
        let encodes = ROW_ENCODINGS.with_borrow(Clone::clone);
        assert_eq!(encodes, vec![removed], "{count} rows");
        assert_eq!(tree.len(), count - 1);
        assert_eq!(tree.cost(), count * (count + 1) / 2 - removed - 1);
        assert_eq!(tree.task_edges(), count / 2 - removed % 2);
        assert_eq!(tree.live_members(&heap.access).unwrap(), count - 1);
        assert_eq!(
            prior.get(&ordinal_key(removed), &heap.access).unwrap(),
            Some(CountedRow(removed))
        );
        assert_eq!(tree.get(&ordinal_key(removed), &heap.access).unwrap(), None);
        for i in [0, count - 1] {
            assert_eq!(
                tree.get(&ordinal_key(i), &heap.access).unwrap(),
                Some(CountedRow(i))
            );
        }
    }
}
