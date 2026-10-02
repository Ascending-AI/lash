//! FIG-4747 on the two-build roll: every process segment admission is an
//! adoption point and records its choice.
//!
//! Build N runs the law's plugin at behaviour revision 1 and build N+1 at
//! revision 2. A three-segment process starts on N; N+1 registers while its
//! segment 1 runs, so its successor is admitted on N+1. A second process is
//! started after the bump.
//!
//! - the process's start records N's composition, and so does segment 1;
//! - the successor's start records N+1's;
//! - the process started after the bump records N+1's from its start;
//! - every entry of a runner into a segment, replays included, is handed the
//!   admission that segment's start recorded.

use super::*;
use lash_core::store::plugin_writers::PluginAdmission;

/// The plugin whose revision the bump moves.
const PLUGIN: &str = "revisioned";

/// A host whose one embedder plugin is [`PLUGIN`] at behaviour `revision`.
fn host(revision: u32) -> lash_core::facade_support::PluginHost {
    let mut declaration = lash_core::plugin::PluginDeclaration::initial(PLUGIN);
    declaration.behavior_revision =
        lash_core::plugin::BehaviorRevision::new(revision).expect("a revision counts from one");
    lash_core::facade_support::PluginHost::new(vec![Arc::new(
        lash_core::plugin::StaticPluginFactory::new(
            declaration,
            lash_core::plugin::PluginSpec::new(),
        ),
    )])
}

/// The revision an admission records for [`PLUGIN`], and that its writer is
/// the plugin's one format.
fn revision(admission: &PluginAdmission) -> u32 {
    let admitted = admission
        .plugins()
        .iter()
        .find(|admitted| admitted.plugin == PLUGIN)
        .expect("the admission names the law's plugin");
    assert_eq!(admitted.writer.get(), 1, "{admission:?}");
    admitted.behavior_revision.get()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_successor_and_a_process_started_after_a_bump_adopt_the_new_composition() {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open the shared SQLite memory store set"),
    );
    // Every attempt replays its journal, so each segment is entered more
    // than once and each entry reads the recorded admission.
    let roll = Roll::start_on(seed(), PROGRAM, false, stores, true).await;
    let seen = PluginLog::default();
    for (runner, revision) in [(&roll.runner_n, 1), (&roll.runner_next, 2)] {
        runner.install_plugins(BuildPlugins {
            host: host(revision),
            fleet: Arc::clone(&roll.sessions),
            seen: Arc::clone(&seen),
        });
    }
    let composition_of = |revision: u32| {
        host(revision)
            .factories()
            .iter()
            .map(|factory| factory.id())
            .collect::<Vec<_>>()
    };

    // The process starts on N, the only build, and N+1 registers from its
    // segment 1: the successor is sent to the newest build.
    let process_id = roll.register_process().await;
    let awaiter = roll.arm_awaiter(&process_id).await;
    roll.runner_n.on_segment(HANDING_OVER, roll.register_next());
    roll.send_segment_zero(&process_id).await;
    let output = tokio::time::timeout(Duration::from_secs(60), awaiter)
        .await
        .expect("the process ends")
        .expect("the awaiter task")
        .expect("the awaiter is answered");
    assert_eq!(
        output,
        process_success(serde_json::json!({ "build": "N+1" })),
        "the successor ran on the newest build"
    );
    roll.settle().await;

    // The process's start is segment 0's admission: N's composition.
    let started = roll
        .record(&process_id)
        .await
        .first_started
        .expect("the process started")
        .plugins
        .expect("the start records its plugin admission");
    assert_eq!(revision(&started), 1);
    assert_eq!(
        started
            .plugins()
            .iter()
            .map(|admitted| admitted.plugin.as_str())
            .collect::<Vec<_>>(),
        composition_of(1),
        "the record is the composition in hook order"
    );

    // Each segment ran under the admission its own start recorded: 0 and 1
    // under N's, the successor under N+1's. Every entry of a segment,
    // replays included, was handed the same record.
    let admitted_as = |process: &ProcessId, ordinal: u64| {
        let entries: Vec<_> = seen
            .lock_recover()
            .iter()
            .filter(|(_, seen_process, seen_ordinal, _)| {
                seen_process == process && *seen_ordinal == ordinal
            })
            .map(|(build, _, _, admission)| {
                (
                    *build,
                    admission.clone().expect("the segment's start recorded one"),
                )
            })
            .collect();
        assert!(!entries.is_empty(), "segment {ordinal} ran");
        assert!(
            entries.iter().all(|entry| *entry == entries[0]),
            "every entry of segment {ordinal} reads one record: {entries:?}"
        );
        entries[0].clone()
    };
    let (build, admission) = admitted_as(&process_id, 0);
    assert_eq!((build, &admission), ("N", &started));
    let (build, admission) = admitted_as(&process_id, HANDING_OVER);
    assert_eq!((build, revision(&admission)), ("N", 1));
    let (build, successor) = admitted_as(&process_id, SUCCESSOR);
    assert_eq!((build, revision(&successor)), ("N+1", 2));
    assert_eq!(
        successor
            .plugins()
            .iter()
            .map(|admitted| admitted.plugin.as_str())
            .collect::<Vec<_>>(),
        composition_of(2)
    );

    // A process started after the bump adopts N+1's composition from its
    // start, and keeps it on every segment.
    let child = roll.register_process().await;
    let awaiter = roll.arm_awaiter(&child).await;
    roll.send_segment_zero(&child).await;
    tokio::time::timeout(Duration::from_secs(60), awaiter)
        .await
        .expect("the later process ends")
        .expect("the awaiter task")
        .expect("the awaiter is answered");
    roll.settle().await;
    let child_started = roll
        .record(&child)
        .await
        .first_started
        .expect("the later process started")
        .plugins
        .expect("the start records its plugin admission");
    assert_eq!(revision(&child_started), 2);
    for ordinal in 0..SEGMENTS {
        let (build, admission) = admitted_as(&child, ordinal);
        assert_eq!(
            (build, revision(&admission)),
            ("N+1", 2),
            "segment {ordinal}"
        );
    }
}
