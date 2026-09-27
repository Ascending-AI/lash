//! The build-generation drain (FIG-3799) compared across the three backends:
//! one scripted sequence of drain marks, per-generation work counts and
//! live-process pages must read back identically.
//!
//! The Postgres database is shared with every other case and every earlier
//! run, so the script's generations are derived from the run nonce and every
//! listing is filtered to them; process ids are minted per backend, so a page
//! is compared by its size and the aliases it names, never by raw ids.

use std::collections::BTreeSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::num::NonZeroUsize;

use lash_core::StoreSet;
use lash_core::engine::BuildGeneration;

use super::*;

const T0: u64 = 2_000_000;

/// One backend's answers to the script, with ids replaced by their aliases.
type Transcript = Vec<String>;

/// A generation unique to this run: the Postgres database outlives it.
fn generation(nonce: &str, alias: &str) -> BuildGeneration {
    let mut hasher = DefaultHasher::new();
    ("fig-3799-drain", nonce, alias).hash(&mut hasher);
    let bytes = hasher.finish().to_be_bytes();
    let mut digest = [0_u8; 6];
    digest.copy_from_slice(&bytes[..6]);
    BuildGeneration::from_digest(digest)
}

#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot answer panics the harness with its name by design"
)]
async fn drain_transcript(stores: &dyn StoreSet, nonce: &str) -> Transcript {
    let drain = stores.generation_drain();
    let registry = stores.process_registry();
    let (a, b) = (generation(nonce, "a"), generation(nonce, "b"));
    let ours = |marked: &BuildGeneration| *marked == a || *marked == b;
    let alias = |marked: &BuildGeneration| if *marked == a { "a" } else { "b" };
    let mut out = vec![
        format!(
            "mark a -> {}",
            drain.mark_draining(&a, T0).await.expect("mark")
        ),
        format!(
            "mark a again -> {}",
            drain.mark_draining(&a, T0 + 5).await.expect("mark")
        ),
        format!(
            "mark b -> {}",
            drain.mark_draining(&b, T0 + 1).await.expect("mark")
        ),
    ];
    let mut marks = drain
        .draining_generations()
        .await
        .expect("list the marks")
        .into_iter()
        .filter(|marked| ours(&marked.generation))
        .map(|marked| format!("{}@{}", alias(&marked.generation), marked.marked_at_ms))
        .collect::<Vec<_>>();
    // Generation order is the listing's contract; the aliases' order is
    // not, so the transcript compares the set and checks the order apart.
    marks.sort();
    out.push(format!("marks -> {marks:?}"));

    // p1 and p2 start under a, p3 under b, p4 never starts.
    let mut aliases = BTreeMap::<lash_sansio::ProcessId, &str>::new();
    for (name, stamp) in [
        ("p1", Some(&a)),
        ("p2", Some(&a)),
        ("p3", Some(&b)),
        ("p4", None),
    ] {
        let process_id = registry
            .register_process(lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            ))
            .await
            .expect("register a drain process")
            .id;
        if let Some(stamp) = stamp {
            let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
                process_id.clone(),
                format!("drain-{name}"),
            )
            .bind_attempt(1);
            let mut started = authority
                .invocation_started()
                .expect("an attempt-bound invocation has a start fact");
            started.build_generation = Some(stamp.clone());
            registry
                .record_first_started_with_authority(&process_id, started, &authority)
                .await
                .expect("start the process under its generation");
        }
        aliases.insert(process_id, name);
    }
    for (name, stamp) in [("a", &a), ("b", &b)] {
        let work = drain.generation_work(stamp).await.expect("count the work");
        out.push(format!("work {name} -> {work:?}"));
    }
    let one = NonZeroUsize::MIN;
    let mut after = None;
    let mut seen = BTreeSet::new();
    loop {
        let page = drain
            .live_processes(&a, after.as_ref(), one)
            .await
            .expect("page the live processes");
        out.push(format!("page of a -> {}", page.len()));
        let Some(last) = page.last().cloned() else {
            break;
        };
        seen.extend(
            page.iter()
                .map(|id| aliases.get(id).copied().unwrap_or("?")),
        );
        after = Some(last);
    }
    out.push(format!("live of a -> {seen:?}"));
    let whole = drain
        .live_processes(&b, None, NonZeroUsize::new(10).unwrap_or(one))
        .await
        .expect("page the live processes")
        .iter()
        .map(|id| aliases.get(id).copied().unwrap_or("?"))
        .collect::<Vec<_>>();
    out.push(format!("live of b -> {whole:?}"));

    out.push(format!(
        "clear a -> {}",
        drain.clear_draining(&a).await.expect("clear")
    ));
    out.push(format!(
        "clear a again -> {}",
        drain.clear_draining(&a).await.expect("clear")
    ));
    let left = drain
        .draining_generations()
        .await
        .expect("list the marks")
        .into_iter()
        .filter(|marked| ours(&marked.generation))
        .map(|marked| alias(&marked.generation))
        .collect::<Vec<_>>();
    out.push(format!("marks after clear -> {left:?}"));
    out.push(format!(
        "clear b -> {}",
        drain.clear_draining(&b).await.expect("clear")
    ));
    out
}

/// The marks list in generation order on every backend.
#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot answer panics the harness with its name by design"
)]
async fn marks_are_in_generation_order(stores: &dyn StoreSet, nonce: &str) {
    let drain = stores.generation_drain();
    let marked = ["c", "d", "e"].map(|alias| generation(nonce, alias));
    for (offset, stamp) in (0_u64..).zip(&marked) {
        drain.mark_draining(stamp, T0 + offset).await.expect("mark");
    }
    let listed = drain
        .draining_generations()
        .await
        .expect("list the marks")
        .into_iter()
        .map(|mark| mark.generation)
        .filter(|listed| marked.contains(listed))
        .collect::<Vec<_>>();
    let mut sorted = listed.clone();
    sorted.sort();
    assert_eq!(listed, sorted, "the marks list in generation order");
    assert_eq!(listed.len(), marked.len(), "every mark lists once");
    for stamp in &marked {
        drain.clear_draining(stamp).await.expect("clear");
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot open panics the harness with its name by design"
)]
pub(super) async fn compare_generation_drains(
    sqlite_root: &Path,
    postgres: &PostgresStorage,
    nonce: &str,
) {
    let memory = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open the SQLite memory drain store set");
    let file = lash_sqlite_store::SqliteStoreSet::open(sqlite_root.join("generation-drains"))
        .await
        .expect("open the SQLite file drain store set");
    let attachments = tempfile::tempdir().expect("attachment directory");
    let postgres_stores = lash_postgres_store::PostgresStoreSet::new(
        postgres,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        )),
    );
    let backends: [(&str, &dyn StoreSet); 3] = [
        ("sqlite-memory", &memory),
        ("sqlite", &file),
        ("postgres", &postgres_stores),
    ];
    let mut observations = Vec::new();
    for (name, stores) in backends {
        observations.push((name, drain_transcript(stores, nonce).await));
        marks_are_in_generation_order(stores, nonce).await;
    }
    for pair in observations.windows(2) {
        let ((left, left_drain), (right, right_drain)) = (&pair[0], &pair[1]);
        assert_eq!(
            left_drain, right_drain,
            "generation drain answers differ between {left} and {right}"
        );
    }
    assert_eq!(
        observations[0].1,
        [
            "mark a -> true",
            "mark a again -> false",
            "mark b -> true",
            &format!("marks -> [\"a@{T0}\", \"b@{}\"]", T0 + 1),
            "work a -> GenerationWork { live_processes: 2, parked_processes: 0, parked_turns: 0 }",
            "work b -> GenerationWork { live_processes: 1, parked_processes: 0, parked_turns: 0 }",
            "page of a -> 1",
            "page of a -> 1",
            "page of a -> 0",
            "live of a -> {\"p1\", \"p2\"}",
            "live of b -> [\"p3\"]",
            "clear a -> true",
            "clear a again -> false",
            "marks after clear -> [\"b\"]",
            "clear b -> true",
        ]
        .map(|line| line.to_owned())
        .to_vec(),
        "the drain script reads back as the port promises"
    );
    eprintln!(
        "PASS generation_drains: backends=3 steps={}",
        observations[0].1.len()
    );
}
