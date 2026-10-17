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
/// shape. A parked cell's retired Annotations node stops its execution
/// root's graph read before the KernelWhole body is decoded (FIG-6376);
/// retired kinds are refused as soon as their envelope is read. A kernel
/// process's state,
/// which carried its sealed KernelWhole graph inline, is refused by the
/// engine that would advance it.
#[tokio::test(flavor = "current_thread")]
async fn pre_heap_images_are_refused_naming_their_format() {
    use lash_core_store::checkpoint_graph::{GraphReadPolicy, GraphRefusal};
    use lash_durable::DurableReads as _;
    use lash_durable::domain::RootOwner;
    let limits = GraphReadPolicy::standard();

    let (stores, _dir) = images::open(&images::PRE_HEAP_SESSION, SimClock::new()).await;
    let reads = stores.durable_store();
    let cells = reads
        .cell_roots_within(
            &lash_protocol_types::SessionId::try_from("v0-session".to_owned()).unwrap(),
            &lash_protocol_types::TurnId::try_from("v0-turn".to_owned()).unwrap(),
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
        matches!(
            &refused,
            lash_durable::DurableError::Graph(GraphRefusal::RetiredFormat {
                kind: 11,
                format: "Annotations",
                ..
            })
        ),
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
    let refused = lash_execution_contracts::process::ProcessEngine::advance(
        engine.as_ref(),
        state,
        lash_execution_contracts::process::EngineEvent::Woke,
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
            lash_execution_contracts::plugin_error::PluginError::StoredDataCorrupt { record_kind, message }
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
            snapshot.formats
                == written[if key.kind() == ActorKind::Session {
                    0
                } else {
                    1
                }],
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
            Some(lash_execution_contracts::formats::TURN_CHECKPOINT_FORMAT_ID)
        }
        DurableFormat::RunRecord => Some(lash_execution_contracts::formats::RUN_RECORD_FORMAT_ID),
        DurableFormat::WaitRow => Some(lash_execution_contracts::formats::WAIT_ROW_FORMAT_ID),
        DurableFormat::OutcomeMaterial => {
            Some(lash_execution_contracts::formats::OUTCOME_MATERIAL_FORMAT_ID)
        }
        DurableFormat::KernelParkedState => Some("kernel-parked-state"),
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
    let mut checked = std::collections::BTreeSet::new();
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
        assert!(
            checked.insert(id),
            "two drain formats claim the same actor format"
        );
    }
    assert_eq!(checked.len(), 5, "the drain formats this build declares");

    // FIG-6687: witness the actual wait writer and a cold typed read, rather
    // than only finding wait-row's spelling in a declaration. The retained
    // baseline actor above does not itself contain a pending wait.
    use lash_durable::domain::{
        DomainWrite, ScopeKey, WaitId, WaitLifecycle, WaitPurpose, WaitRow, WaitWrite,
    };
    use lash_durable::{CommitLabel, DurableInstant, FormatSet, MailTx, NodeSpec};
    let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
    let store = stores.durable_store();
    let owner = ActorKey::session("format-wait-writer").unwrap();
    let formats: FormatSet = sets[0].clone();
    let mut mail = MailTx::new();
    mail.create_actor(owner.clone(), formats.clone());
    store
        .commit_mail(mail, CommitLabel::new("format.wait.create"))
        .await
        .unwrap();
    let node = store
        .register_node(&NodeSpec {
            node: lash_durable::NodeId::new("format-writer"),
            decodes: vec![formats.clone()],
            serves: Vec::new(),
            ttl_millis: 60_000,
        })
        .await
        .unwrap();
    let epoch = store.claim(&node, 1).await.unwrap()[0].epoch;
    let id = WaitId([0x67; 16]);
    let scope = ScopeKey::Session(
        lash_protocol_types::SessionId::try_from("format-wait-writer".to_owned()).unwrap(),
    );
    let purpose = WaitPurpose::Timer {
        deadline: DurableInstant(1_900_000_000_000),
    };
    let mut tx = store.begin(&owner, epoch).await.unwrap();
    tx.write(DomainWrite::Wait(WaitWrite::Pin {
        id,
        scope: scope.clone(),
        purpose: purpose.clone(),
    }));
    store
        .commit(tx, CommitLabel::new("format.wait.write"))
        .await
        .unwrap();
    let peer = stores.reopen_with_clock(clock as _).await.unwrap();
    use lash_durable::DurableReads as _;
    assert_eq!(
        peer.durable_store().wait(&id).await.unwrap(),
        Some(WaitRow {
            id,
            owner: owner.clone(),
            scope,
            purpose,
            lifecycle: WaitLifecycle::Pending,
            created_epoch: epoch
        })
    );
    assert_eq!(
        peer.durable_store()
            .actor(&owner)
            .await
            .unwrap()
            .unwrap()
            .formats,
        formats
    );
}

/// FIG-6474: every witness owns its archive file, so another recorder cannot
/// rewrite it as part of a shared corpus.
#[test]
fn a_retained_image_owns_its_archive_file() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats/corpus");
    for set in images::retained() {
        for image in &set.images {
            let path = root.join(&set.build).join(format!("{}.json", image.name));
            assert!(
                path.is_file(),
                "{} has no independent archive file",
                path.display()
            );
        }
    }
}

/// FIG-6474: both corpus and baseline recorders may write only the selected
/// witness's files.
#[test]
fn a_recorder_writes_only_its_selected_image() {
    fn snapshot(
        root: &std::path::Path,
    ) -> std::collections::BTreeMap<std::path::PathBuf, (Vec<u8>, std::time::SystemTime)> {
        let mut files = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                for (name, value) in snapshot(&path) {
                    files.insert(
                        std::path::Path::new(path.file_name().unwrap()).join(name),
                        value,
                    );
                }
            } else {
                files.insert(
                    path.file_name().unwrap().into(),
                    (
                        std::fs::read(&path).unwrap(),
                        std::fs::metadata(&path).unwrap().modified().unwrap(),
                    ),
                );
            }
        }
        files
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Start with every retained entry, so the write-set includes other shapes
    // in the same build and a different build's witnesses.
    for set in images::retained() {
        let bytes = serde_json::to_vec(set).unwrap();
        images::append_at(root, serde_json::from_slice(&bytes).unwrap());
        let mut other: images::ImageSet = serde_json::from_slice(&bytes).unwrap();
        other.build = format!("other-{}", set.build);
        images::append_at(root, other);
    }
    for image in [
        &images::SESSION,
        &images::LASH_VM,
        &images::PRE_HEAP_SESSION,
        &images::PRE_HEAP_LASH_VM,
    ] {
        let out = root.join(image.name.as_ref());
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("db.gz"), &image.db).unwrap();
        std::fs::write(out.join("db-wal.gz"), &image.wal).unwrap();
    }
    for set in images::retained() {
        for image in &set.images {
            let before = snapshot(root);
            let mut replacement: images::RetainedImage =
                serde_json::from_slice(&serde_json::to_vec(image).unwrap()).unwrap();
            // Both carriers must change: DB replacement alone cannot witness a
            // recorder that silently keeps the old WAL (FIG-6687 / FIG-6474).
            replacement.db_gzip_base64 = format!("replacement database for {}", image.name);
            replacement.wal_gzip_base64 = format!("replacement WAL for {}", image.name);
            let expected = serde_json::to_value(&replacement).unwrap();
            images::rerecord_at(root, &set.build, replacement);
            let mut after = snapshot(root);
            let selected = std::path::PathBuf::from("corpus")
                .join(&set.build)
                .join(format!("{}.json", image.name));
            let recorded: serde_json::Value = serde_json::from_slice(&after[&selected].0).unwrap();
            assert_eq!(
                recorded["images"][0], expected,
                "both selected carriers and the old oracle survive"
            );
            after.insert(selected.clone(), before[&selected].clone());
            assert_eq!(
                before, after,
                "refreshing {} touched another witness",
                image.name
            );
            let before = snapshot(root);
            let mut changed_oracle: images::RetainedImage =
                serde_json::from_value(expected).unwrap();
            changed_oracle.expected = serde_json::json!({"changed": "oracle"});
            assert!(
                std::panic::catch_unwind(|| images::rerecord_at(root, &set.build, changed_oracle))
                    .is_err()
            );
            assert_eq!(
                before,
                snapshot(root),
                "a changed oracle is refused before any write"
            );
        }
    }
    let store = tempfile::tempdir().unwrap();
    let path = images::database_path(store.path());
    std::fs::write(&path, b"new database bytes").unwrap();
    std::fs::write(path.with_extension("db-wal"), b"new WAL bytes").unwrap();
    for image in images::BASELINE {
        let before = snapshot(root);
        images::regenerate_at(root, &path, image.name.as_ref());
        let mut after = snapshot(root);
        for file in ["db.gz", "db-wal.gz"] {
            let selected = std::path::PathBuf::from(image.name.as_ref()).join(file);
            use std::io::Read as _;
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(after[&selected].0.as_slice())
                .read_to_end(&mut decoded)
                .unwrap();
            let expected: &[u8] = if file == "db.gz" {
                b"new database bytes"
            } else {
                b"new WAL bytes"
            };
            assert_eq!(
                decoded, expected,
                "the selected baseline carrier is replaced exactly"
            );
            after.insert(selected.clone(), before[&selected].clone());
        }
        assert_eq!(
            before, after,
            "recording {} touched another witness",
            image.name
        );
    }
}
