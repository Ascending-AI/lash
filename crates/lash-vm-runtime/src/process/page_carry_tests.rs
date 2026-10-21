#![allow(clippy::expect_used, clippy::unwrap_used)]
//! K-VER-005, K-LIB-009 and ORIGIN against actual parked page records.
use lash_core_store::checkpoint_graph::{EncodedNode, NodeId};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::{Datum, Document, FunctionDefinition, FunctionRegistry};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_vm::{KernelMachine, Machine, Program, Step};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone, Default)]
struct Pages(Rc<RefCell<BTreeMap<NodeId, EncodedNode>>>);
impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|page| {
                let nodes = self.0.borrow();
                let bytes = &nodes
                    .get(&NodeId(page.id.0))
                    .ok_or(HeapFault::Missing { page: page.id })?
                    .bytes;
                lash_vm_broker::kernel::pages::answer_page(*page, bytes, |_| Ok(PageGrant::NONE))
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let node = lash_vm_broker::kernel::pages::frame_page(&body)?
            .named()
            .into_node();
        let reference = PageRef {
            id: PageId(node.id.0),
            grant: PageGrant::NONE,
        };
        self.0.borrow_mut().insert(node.id, node);
        Ok(reference)
    }
}
struct Host;
impl lash_kernel_vm::Host for Host {
    fn clock(&mut self) -> lash_kernel_doc::Timestamp {
        panic!("no clock")
    }
    fn random(&mut self) -> u64 {
        panic!("no random")
    }
    fn read(
        &mut self,
        _: &lash_kernel_doc::Handle,
        _: &Datum,
    ) -> Result<Datum, lash_kernel_doc::ErrorDatum> {
        panic!("no read")
    }
    fn print(&mut self, _: &Datum) {
        panic!("no print")
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
fn bounds() -> lash_kernel_vm::Bounds {
    lash_kernel_vm::Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 1_000_000,
        memory: 64 << 20,
        call_depth: 100,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
    }
}
fn release(next: bool) -> Vec<FunctionDefinition> {
    let extra = if next { "let future = null" } else { "" };
    let fetch = lash_kernel_doc::parse_definition(&format!(
        "function pkg.fetch(x: Any) -> Any\nkernel 1\ncharge 1\neffect echo(x?: Any) -> Any contract \"1\"\nbody {{ let v = perform echo(x) as Any {extra} return v }}"
    )).unwrap();
    let start = lash_kernel_doc::parse_definition(&format!(
        "function pkg.start(x: Any) -> Any\nkernel 1\ncharge 1\nuse pkg.fetch = @{}\nbody {{ let h = spawn invoke pkg.fetch(x) return h }}", fetch.identity()
    )).unwrap();
    vec![fetch, start]
}
fn document(definitions: &[FunctionDefinition]) -> Document {
    lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers float\neffect echo(x?: Any) -> Any contract \"1\"\nuse pkg.fetch = @{}\nuse pkg.start = @{}\nmain {{ let captured = [7] let f = fn() {{ return captured[0] }} let h = invoke pkg.start(7) let v = join h let saved = apply f() return [v, saved] }}",
        definitions[0].identity(), definitions[1].identity()
    )).unwrap()
}
fn registry(definitions: &[FunctionDefinition]) -> Arc<FunctionRegistry> {
    let mut functions = FunctionRegistry::new();
    for definition in definitions {
        functions.register(definition.clone(), None).unwrap();
    }
    Arc::new(functions)
}
fn program(document: Document, functions: Arc<FunctionRegistry>, closure: PageId) -> Program {
    Program {
        document: Arc::new(document),
        library: lash_kernel_vm::PreparedLibrary::new(functions),
        closure,
    }
}
fn park(
    document: Document,
    functions: Arc<FunctionRegistry>,
    pages: &Pages,
) -> (
    KernelMachine,
    lash_kernel_vm::SealedRoot,
    lash_kernel_vm::EffectRequest,
) {
    park_with(document, functions, pages, PageId([3; 32]), PageId([4; 32]))
}
fn park_with(
    document: Document,
    functions: Arc<FunctionRegistry>,
    pages: &Pages,
    closure: PageId,
    owner: PageId,
) -> (
    KernelMachine,
    lash_kernel_vm::SealedRoot,
    lash_kernel_vm::EffectRequest,
) {
    let mut machine = KernelMachine::start(
        program(document, functions, closure),
        bounds(),
        lash_kernel_vm::CellStart {
            owner,
            target: lash_kernel_vm::Target::Main,
            args: Vec::new(),
            session: None,
            modes: lash_kernel_vm::EffectModes::recorded(),
            host: Vec::new(),
        },
        Box::new(pages.clone()),
    )
    .unwrap();
    let Step::Parked(park) = machine.run(&mut Host, u64::MAX).unwrap() else {
        panic!("park")
    };
    let lash_kernel_vm::Request::Effect(request) = park.requests[0].clone() else {
        panic!("effect")
    };
    let root = machine.checkpoint().unwrap();
    (machine, root, request)
}
fn finish(machine: &mut KernelMachine, wait: lash_kernel_vm::WaitId) -> Datum {
    machine
        .deliver(
            wait,
            lash_kernel_vm::Outcome::Completed(Datum::Int(7.into())),
        )
        .unwrap();
    match machine.run(&mut Host, u64::MAX).unwrap() {
        Step::Ended(lash_kernel_vm::End::Finished(result)) => result.result,
        other => panic!("{other:?}"),
    }
}
/// K-LIB-009/K-VER-005/ORIGIN: a library-spawned child parked inside A's
/// body, its ancestry and authored origin, a shared capture, and every
/// pending frame move to B. A is absent from the resuming registry.
#[test]
fn a_parked_library_spawn_and_captured_closure_resume_after_retirement() {
    let (a, b) = (release(false), release(true));
    let mut both = a.clone();
    both.extend(b.clone());
    let pages = Pages::default();
    let (mut original, root, request) = park(document(&a), registry(&a), &pages);
    let rewritten = lash_kernel_migrate::adopt(
        &document(&a),
        &lash_kernel_migrate::relink(&a, &b),
        registry(&both).as_ref(),
    )
    .unwrap();
    let (_, carried) = lash_kernel_migrate::carry_checkpoint(
        &mut pages.clone(),
        &root.checkpoint(),
        &rewritten,
        PageId([5; 32]),
        (1, root.checkpoint().header().closure),
    )
    .unwrap();
    let program = program(rewritten.document.clone(), registry(&b), PageId([5; 32]));
    lash_kernel_vm::validate_foreign_checkpoint(
        program.clone(),
        bounds(),
        &carried,
        Box::new(pages.clone()),
    )
    .unwrap();
    let mut resumed = KernelMachine::open(program, bounds(), &carried, Box::new(pages)).unwrap();
    assert_eq!(root.checkpoint().header().charged, carried.header().charged);
    assert_eq!(root.checkpoint().header().limits, carried.header().limits);
    assert_eq!(
        finish(&mut resumed, request.wait),
        finish(&mut original, request.wait)
    );
}
/// K-VER-005: a held coordinate without a successor refuses at that site;
/// the original page stays readable and resumes to its original result.
#[test]
fn an_unmapped_spawn_origin_refuses_without_changing_the_original_run() {
    let a = release(false);
    let pages = Pages::default();
    let (mut machine, root, request) = park(document(&a), registry(&a), &pages);
    let mut rewritten =
        lash_kernel_migrate::adopt(&document(&a), &BTreeMap::new(), registry(&a).as_ref()).unwrap();
    rewritten.correspondence = lash_kernel_doc::Correspondence::of(
        rewritten.correspondence.base,
        rewritten.correspondence.result,
        rewritten
            .correspondence
            .entries()
            .iter()
            .filter(|entry| entry.from != request.origin)
            .cloned()
            .collect(),
    )
    .unwrap();
    assert!(matches!(
        lash_kernel_migrate::carry_checkpoint(
            &mut pages.clone(),
            &root.checkpoint(),
            &rewritten,
            root.checkpoint().header().closure,
            (1, root.checkpoint().header().closure)
        ),
        Err(lash_kernel_migrate::CarryFault::Refused(
            lash_kernel_migrate::ParkedRefusal::SiteNotCarried { site, held }
        )) if site == request.origin && held.as_ref() == "spawn origin"
    ));
    assert_eq!(
        finish(&mut machine, request.wait),
        Datum::List(vec![Datum::Int(7.into()), Datum::Int(7.into())])
    );
}

/// FIG-6127/R1: kernel carry derives the successor of the exact pinned
/// code, even when another release with the same names is also retained.
#[cfg(feature = "synthetic-next")]
#[test]
fn a_kernel_format_carry_keeps_exact_pinned_code_over_another_retained_release() {
    let (a, b) = (release(false), release(true));
    let mut both = a.clone();
    both.extend(b);
    let mut functions = FunctionRegistry::clone(&registry(&both));
    let migration =
        lash_kernel_migrate::migration_from(lash_kernel_doc::KernelVersion::One).unwrap();
    lash_kernel_migrate::migrate_registry(&mut functions, migration).unwrap();
    let pages = Pages::default();
    let (mut original, root, request) = park(document(&a), registry(&a), &pages);
    let plan = super::migrate::plan_migration(&document(&a), &functions)
        .unwrap()
        .unwrap();
    let (_, carried) = lash_kernel_migrate::carry_checkpoint(
        &mut pages.clone(),
        &root.checkpoint(),
        &plan.rewritten,
        PageId([5; 32]),
        (1, root.checkpoint().header().closure),
    )
    .unwrap();
    let program = program(
        plan.rewritten.document,
        Arc::new(functions),
        PageId([5; 32]),
    );
    lash_kernel_vm::validate_foreign_checkpoint(
        program.clone(),
        bounds(),
        &carried,
        Box::new(pages.clone()),
    )
    .unwrap();
    let mut resumed = KernelMachine::open(program, bounds(), &carried, Box::new(pages)).unwrap();
    assert_eq!(
        carried.header().kernel,
        lash_kernel_doc::KernelVersion::NEWEST.number()
    );
    assert_eq!(
        finish(&mut resumed, request.wait),
        finish(&mut original, request.wait)
    );
}

/// A foreign header cannot be relabelled as the migration's source: kernel
/// and exact closure are checked before any page is rewritten.
#[test]
fn a_foreign_kernel_or_closure_refuses_before_rewriting() {
    let a = release(false);
    let pages = Pages::default();
    let (_, root, _) = park(document(&a), registry(&a), &pages);
    let rewrite =
        lash_kernel_migrate::adopt(&document(&a), &BTreeMap::new(), registry(&a).as_ref()).unwrap();
    let before = pages.0.borrow().len();
    for pinned in [
        (2, root.checkpoint().header().closure),
        (1, PageId([99; 32])),
    ] {
        assert!(
            lash_kernel_migrate::carry_checkpoint(
                &mut pages.clone(),
                &root.checkpoint(),
                &rewrite,
                PageId([5; 32]),
                pinned
            )
            .is_err()
        );
        assert_eq!(pages.0.borrow().len(), before);
    }
}

/// FIG-6127: a retained R2 never substitutes for absent R1, and an R1
/// surface that does not supply a pinned identity refuses naming both.
#[tokio::test]
async fn an_absent_or_incompatible_pinned_snapshot_refuses_without_selecting_another() {
    let package = lash_kernel_package::Package::build(
        "pkg",
        "R2",
        &lash_kernel_package::PackageSources {
            kernel: "function pkg.value() -> Any\nkernel 1\ncharge 1\nbody { return null }\n",
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    let held = lash_kernel_package::HeldPackages::install([package]).unwrap();
    let absent = lash_kernel_doc::PackageSetDigest::from_bytes([97; 32]);
    let missing = release(false)[0].identity();
    for pinned in [absent, held.digest()] {
        let error = super::page_carry::carry_snapshot(
            &crate::NoInternalOperations,
            &held,
            pinned,
            [missing],
        )
        .await
        .unwrap_err();
        let lash_kernel_migrate::CarryFault::Refused(
            lash_kernel_migrate::ParkedRefusal::StateNotCarried { detail },
        ) = error
        else {
            panic!("{error}");
        };
        assert!(detail.contains(&pinned.to_string()), "{detail}");
        if pinned != absent {
            assert!(detail.contains(&missing.to_string()), "{detail}");
        }
    }
}
