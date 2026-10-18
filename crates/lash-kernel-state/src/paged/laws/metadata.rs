//! Bounded S1/S4 metadata and exact diagnostic projections.
use super::super::directory::ordinal_key;
use super::super::tree::{SealWork, Tree};
use super::super::*;
use super::heap::{fresh, seal};
use lash_kernel_doc::graph_tokens::{decode_body, encode_body};
use lash_kernel_doc::heap::*;
use lash_kernel_doc::kernel_internals::TaskIdentity;
use lash_kernel_doc::{Datum, Name, Site, Unit};

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
        .allocate_object(ObjectInit::Variable(PageValue::Null))
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
                value: BoundRecord::Value(PageValue::Closure(closure)),
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
        heap.set_execution_roots(tasks.clone(), Tree::new(false), &[], &[])
            .unwrap();
        tasks.seal(&heap.access, &mut work()).unwrap();
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
    let value = PageValue::Function(descriptor.clone());
    let resident = PageValue::Function(TextRef {
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
        heap.normal_key(&PageValue::Text(descriptor)).unwrap(),
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
    let decoded: PageValue = decode_body(&encode_body(&member)).unwrap();
    assert!(matches!(
        decoded,
        PageValue::Function(TextRef {
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
