//! L11 (FIG-5187): the 1.0 decode-and-resume fixtures (ADR 0106 §1, §6).
//!
//! A fixture is the SQLite store a build left behind at a committed phase,
//! committed as encoded bytes (`support/images.rs`). Its actor's state is
//! written in the shipped build's format set for its kind, so it holds that
//! set's formats at their 1.0 shape. A resume test opens a copy on a fresh
//! node of this build, which claims the actor, decodes its state and
//! reaches the expected next commit:
//!
//! - `lashvm` (here): a kernel process whose entry `worker()` sleeps 5,000 ms,
//!   sleeps 7,000 ms and returns `"done"`, parked on its first sleep and released
//!   by a draining node, then woken `ready`. Its state is the kernel engine's state
//!   around the machine's parked run.
//! - `session` (`vertical_crash_proof.rs`): V0's turn, cut by a crash right
//!   after its cell's `Once` operation's outcome committed: the turn
//!   checkpoint, the cell's kernel continuation parked on the operation,
//!   the operation's run records and its outcome's material.
//!
//! The baseline coverage check requires an image for each shipped actor format
//! set. The append-only realistic parked-run corpus and its recording policy
//! live beside these images in `fixtures/formats/README.md`; its law runs in
//! `vertical_crash_proof::corpus`. Never re-record an old upgrade witness to
//! repair a resume failure.

// Test code: the generator reads its workspace from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/images.rs"]
mod images;
#[path = "support/sim.rs"]
mod sim;

#[path = "support/kernel_images.rs"]
mod kernel_images;
use images::Image;
use kernel_images::{actor, build};
use lash_durable::{ActorKey, ActorKind, DurableStore};
use lash_durable_test::SimClock;
use lash_vm_runtime::LASH_VM_ENGINE_KIND;
use std::sync::Arc;

#[tokio::test(flavor = "current_thread")]
#[ignore = "regenerates crates/lash-durable-test/tests/fixtures/formats/lashvm"]
async fn regenerate_kernel_process_format_fixture() {
    let (_dir, path) = kernel_images::record(kernel_images::WORKER).await;
    images::regenerate(&path, images::LASH_VM.name.as_ref());
}

#[tokio::test(flavor = "current_thread")]
async fn a_kernel_process_decodes_and_resumes_from_its_1_0_image() {
    let end = kernel_images::resume(&images::LASH_VM).await;
    assert!(end.to_string().contains("\"done\""), "{end}");
}

/// FIG-6193: a pre-heap image is refused with a typed refusal naming its
/// format, never misread. The persistent heap replaced the whole-state
/// parked run in place under the 1.0 freeze, with no reader for the old
/// shape. A parked cell's KernelWhole root stops its execution root's
/// graph read before any of its body is decoded; a kernel process's state,
/// which carried its sealed KernelWhole graph inline, is refused by the
/// engine that would advance it.
#[tokio::test(flavor = "current_thread")]
async fn pre_heap_images_are_refused_naming_their_format() {
    use lash_core_store::checkpoint_graph::{GraphReadPolicy, GraphRefusal};
    use lash_durable::DurableReads as _;
    use lash_durable::domain::RootOwner;
    let limits = GraphReadPolicy::standard();
    let kernel_whole = |refusal: &GraphRefusal| {
        matches!(
            refusal,
            GraphRefusal::RetiredFormat {
                kind: 16,
                format: "KernelWhole",
                ..
            }
        )
    };

    let (stores, _dir) = images::open(&images::PRE_HEAP_SESSION, SimClock::new()).await;
    let reads = stores.durable_store();
    let cells = reads
        .cell_roots_within(
            &lash_sansio::SessionId::try_from("v0-session".to_owned()).unwrap(),
            &lash_sansio::TurnId::try_from("v0-turn".to_owned()).unwrap(),
            8,
        )
        .await
        .expect("the image's cell roots list");
    assert_eq!(cells.len(), 1, "the image parks one cell");
    let refused = reads
        .hydrate(
            RootOwner::Exec(cells[0].exec.clone()),
            Some(cells[0].root),
            limits.execution(),
        )
        .await
        .map(|_| ())
        .expect_err("a pre-heap parked cell is never read");
    assert!(
        matches!(&refused, lash_durable::DurableError::Graph(refusal) if kernel_whole(refusal)),
        "{refused:?}"
    );

    let clock = SimClock::new();
    let (stores, _dir) = images::open(&images::PRE_HEAP_LASH_VM, Arc::clone(&clock)).await;
    let (_, engine) = build(Arc::new(stores.clone()), &clock);
    let (_, written_by, state) =
        lash_core_execution::runtime::actor::process::state_graph::read_engine_state(
            &stores.durable_store(),
            &kernel_images::process(),
            limits.engine(),
        )
        .await
        .expect("the process's execution root reads")
        .expect("the image parks the process");
    assert_eq!(written_by, LASH_VM_ENGINE_KIND);
    let refused = lash_core_execution::ProcessEngine::advance(
        engine.as_ref(),
        state,
        lash_core_execution::EngineEvent::Woke,
    )
    .map(|_| ())
    .expect_err("a pre-heap parked run is never advanced")
    .into_plugin_error();
    let named = GraphRefusal::RetiredFormat {
        node: None,
        kind: 16,
        format: "KernelWhole",
    }
    .to_string();
    assert!(
        matches!(
            &refused,
            lash_core_execution::PluginError::StoredDataCorrupt { record_kind, message }
                if record_kind == "kernel process engine state" && *message == named
        ),
        "{refused:?}"
    );
}

/// Fixture coverage: every set the shipped build writes has a committed
/// image whose actor is stored in it, and every image is in one of them.
#[tokio::test(flavor = "current_thread")]
async fn every_format_set_the_build_writes_has_a_decode_and_resume_fixture() {
    let clock = SimClock::new();
    let probe = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(Arc::clone(&clock) as _)
            .await
            .expect("an in-memory store set opens"),
    );
    let (backend, _) = build(probe, &clock);
    let mut written = vec![backend.formats().session().clone()];
    written.push(
        backend
            .formats()
            .process(LASH_VM_ENGINE_KIND)
            .expect("the build has the kernel engine")
            .clone(),
    );
    let actors: [(&Image, ActorKey); 2] = [
        (
            &images::SESSION,
            ActorKey::session("v0-session").expect("a session actor key"),
        ),
        (&images::LASH_VM, actor()),
    ];
    assert_eq!(
        actors.len(),
        images::BASELINE.len(),
        "an image has no coverage entry"
    );
    let mut covered = Vec::new();
    for (image, key) in &actors {
        let (stores, _dir) = images::open(image, Arc::clone(&clock)).await;
        let snapshot = stores
            .durable_store()
            .actor(key)
            .await
            .expect("the image reads")
            .unwrap_or_else(|| panic!("image {} holds no actor {key}", image.name));
        assert!(
            written.contains(&snapshot.formats),
            "image {} is in {}, which this build does not write",
            image.name,
            snapshot.formats
        );
        assert_eq!(
            key.kind(),
            if snapshot.formats == written[0] {
                ActorKind::Session
            } else {
                ActorKind::Process
            }
        );
        covered.push(snapshot.formats);
    }
    for set in &written {
        assert!(
            covered.contains(set),
            "no decode-and-resume fixture is in {set}"
        );
    }
}

/// The id a drain format has in the actor format sets, or `None` for one
/// that no actor's state holds.
fn actor_format_id(format: lash::formats::DurableFormat) -> Option<String> {
    use lash::formats::DurableFormat;
    match format {
        DurableFormat::TurnCheckpoint => {
            Some(lash_core_execution::formats::TURN_CHECKPOINT_FORMAT_ID)
        }
        DurableFormat::RunRecord => Some(lash_core_execution::formats::RUN_RECORD_FORMAT_ID),
        DurableFormat::WaitRow => Some(lash_core_execution::formats::WAIT_ROW_FORMAT_ID),
        DurableFormat::OutcomeMaterial => {
            Some(lash_core_execution::formats::OUTCOME_MATERIAL_FORMAT_ID)
        }
        _ => None,
    }
    .map(str::to_owned)
}

/// Every persisted durable format whose stored bytes drain rather than
/// migrate is in an actor's format set at the version this build writes:
/// moving its version moves the set, and the claim filter keeps an actor
/// written in it from a node that cannot decode it (ADR 0106 §1).
#[tokio::test(flavor = "current_thread")]
async fn every_drain_format_is_in_an_actor_format_set() {
    use lash::formats::{FormatProbe, FormatVersion, UpgradePolicy};

    let clock = SimClock::new();
    let probe = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(Arc::clone(&clock) as _)
            .await
            .expect("an in-memory store set opens"),
    );
    let (backend, _) = build(probe, &clock);
    let sets = [
        backend.formats().session().clone(),
        backend
            .formats()
            .process(LASH_VM_ENGINE_KIND)
            .expect("the build has the kernel engine")
            .clone(),
    ];
    let members: Vec<&str> = sets
        .iter()
        .flat_map(|set| {
            let (_, members) = set.as_str().split_once(':').expect("a kind prefix");
            members.split(',')
        })
        .collect();
    let mut checked = 0;
    for entry in lash::formats::durable_formats() {
        if entry.format.upgrade_policy() != UpgradePolicy::Drain
            || entry.probe == FormatProbe::NotPersisted
        {
            continue;
        }
        let name = entry.format.name();
        let id = actor_format_id(entry.format)
            .unwrap_or_else(|| panic!("the drain format {name} is in no actor's format set"));
        let FormatVersion::Counter(version) = entry.version else {
            panic!("the drain format {name} has no counter");
        };
        let member = format!("{id}@{version}");
        assert!(
            members.contains(&member.as_str()),
            "the drain format {name} ({member}) is in no set: {sets:?}"
        );
        checked += 1;
    }
    assert_eq!(checked, 4, "the drain formats this build declares");
}
