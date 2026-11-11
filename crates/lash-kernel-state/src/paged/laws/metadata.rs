//! Bounded S1/S4 metadata and exact diagnostic projections.
use super::super::tree::{SealWork, Tree};
use super::super::*;
use super::pages::fresh;

use lash_kernel_doc::heap::*;
use lash_kernel_value::names::Name;
use lash_kernel_value::sites::Site;
use lash_kernel_value::sites::TaskIdentity;
use lash_kernel_value::sites::Unit;
use {lash_kernel_value::tokens::decode_body, lash_kernel_value::tokens::encode_body};

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
    let (access, pages) = fresh();
    let name = Name::new("x".repeat(1 << 20));
    let named = NameRef::new(&name, &access).unwrap();
    let site = SiteRef::new(&Site::new(Unit::Function(name.clone()), [0]), &access).unwrap();
    let key = metadata::metadata_key(&name);
    let mut names = Tree::new(false);
    names.write(key.clone(), named, &access).unwrap();
    let before = pages.borrow().seals;
    names.seal(&access, &mut work()).unwrap();
    assert!(
        pages.borrow().seals - before <= 2,
        "the name's row, not its payload"
    );
    let restored = Tree::<NameRef>::open(
        Some(names.page().unwrap()),
        names.len(),
        LogicalBytes(names.cost()),
        false,
    );
    access.clear();
    let opened = restored.get(&key, &access).unwrap().unwrap();
    assert_eq!(opened.resolve(&access).unwrap(), name);
    assert_eq!(
        site.resolve(&access).unwrap().unit,
        Unit::Function(name.clone())
    );
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
    let (access, pages) = fresh();
    let site = Site::new(Unit::Main, [7]);
    let saved_site = SiteRef::new(&site, &access).unwrap();
    let mut identity = TaskIdentityRef::MAIN;
    for occurrence in 0..10_000 {
        let before = pages.borrow().seals;
        identity =
            TaskIdentityRef::spawn(identity, saved_site.clone(), occurrence, &access).unwrap();
        assert!(
            pages.borrow().seals - before <= 1,
            "one parent node per spawn"
        );
        let node = identity.root.expect("a spawned identity names its node");
        assert!(pages.borrow().pages[&node.id].tokens.len() < 512);
    }
    let mut projected = identity.resolve(&access).unwrap();
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

/// S1 fault law: a decoded-name cache must retain the descriptor's summary
/// validation, even when the referenced payload was previously read.
#[test]
fn a_cached_metadata_name_does_not_hide_a_bad_payload_summary() {
    let (access, _) = fresh();
    let first = NameRef::new(&Name::new("n".repeat(1 << 20)), &access).unwrap();
    first.resolve(&access).unwrap();
    let mut text: TextRef = decode_body(&encode_body(&first)).unwrap();
    text.summary.utf8_bytes += 1;
    text.summary.utf16_units += 1;
    text.summary.chars += 1;
    let bad: NameRef = decode_body(&encode_body(&text)).unwrap();
    assert!(matches!(
        bad.resolve(&access),
        Err(HeapFault::Corrupt { .. })
    ));
}
