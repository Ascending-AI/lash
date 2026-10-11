//! Group commit laws (FIG-5958). Each law queues its writes on the
//! connection thread behind a write that holds it, so the writes form one
//! batch deterministically when it lets go.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::Member;
use crate::conn::tests::installed;
use crate::conn::{
    FencedTx, SqliteConnection, SqliteConnectionPolicy, SqliteSynchronous, TxOutcome,
};
use crate::location::DatabaseTarget;

/// A `Full` writer on a fresh file database with batch limits `members` and
/// `rows`, its `rows` table, and an independent connection that reads what
/// is committed.
struct World {
    _dir: tempfile::TempDir,
    writer: SqliteConnection,
    outside: Arc<Mutex<rusqlite::Connection>>,
    commits: Arc<AtomicUsize>,
}

async fn world(members: usize, rows: usize) -> World {
    let dir = tempfile::tempdir().expect("batch law directory");
    let path = dir.path().join("batch.db");
    let mut policy = SqliteConnectionPolicy::standard(SqliteSynchronous::Full);
    policy.operational.write_batch_members = std::num::NonZeroUsize::new(members).expect("members");
    policy.operational.write_batch_rows = std::num::NonZeroUsize::new(rows).expect("rows");
    let writer = installed(&DatabaseTarget::File(path.clone()), policy).await;
    writer
        .write(|tx| {
            tx.execute_batch("CREATE TABLE rows (member TEXT NOT NULL)")?;
            Ok(())
        })
        .await
        .expect("create the rows table");
    let commits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&commits);
    writer
        .call(move |c| {
            c.commit_hook(Some(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                false
            }));
            Ok(())
        })
        .await
        .expect("count the writer's commits");
    let outside = rusqlite::Connection::open(&path).expect("an independent reader");
    World {
        _dir: dir,
        writer,
        outside: Arc::new(Mutex::new(outside)),
        commits,
    }
}

impl World {
    /// Hold the connection thread inside a write of its own until the
    /// returned sender is dropped. The write's batch is gathered before it
    /// runs, so everything queued while it holds forms the next batch.
    async fn hold(&self) -> std::sync::mpsc::Sender<()> {
        let (entered, held) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        self.queue(
            move |_| {
                entered.send(()).expect("signal the hold");
                let _ = released.recv();
                Ok(TxOutcome::Commit(()))
            },
            |_: rusqlite::Result<()>| {},
        );
        held.await.expect("the connection thread holds");
        release
    }

    /// Queue `body` now, in this order, answering through `reply`.
    fn queue<T, F, R>(&self, body: F, reply: R)
    where
        T: Send + 'static,
        F: FnOnce(&FencedTx<'_>) -> rusqlite::Result<TxOutcome<T>> + Send + 'static,
        R: FnOnce(rusqlite::Result<T>) + Send + 'static,
    {
        self.writer
            .inner
            .write(Member::new(&self.writer.writer, body, reply))
            .expect("queue a write");
    }

    /// The committed rows, in commit order, as an independent connection
    /// reads them.
    fn committed(&self) -> Vec<String> {
        committed(&self.outside)
    }
}

fn committed(outside: &Mutex<rusqlite::Connection>) -> Vec<String> {
    let outside = outside.lock().expect("reader");
    let mut statement = outside
        .prepare("SELECT member FROM rows ORDER BY rowid")
        .expect("read rows");
    statement
        .query_map([], |row| row.get(0))
        .expect("rows")
        .collect::<rusqlite::Result<_>>()
        .expect("decode rows")
}

fn insert(tx: &FencedTx<'_>, member: &str) -> rusqlite::Result<()> {
    tx.execute("INSERT INTO rows (member) VALUES (?1)", [member])
        .map(drop)
}

/// What each member answered, and what an independent reader saw committed
/// at that moment.
type Answers = Arc<Mutex<Vec<(String, Result<String, String>, Vec<String>)>>>;

fn reply<T: std::fmt::Debug>(
    answers: &Answers,
    outside: &Arc<Mutex<rusqlite::Connection>>,
    member: &str,
) -> impl FnOnce(rusqlite::Result<T>) + Send + 'static {
    let answers = Arc::clone(answers);
    let outside = Arc::clone(outside);
    let member = member.to_owned();
    move |result| {
        let seen = committed(&outside);
        answers.lock().expect("answers").push((
            member,
            result
                .map(|value| format!("{value:?}"))
                .map_err(|error| error.to_string()),
            seen,
        ));
    }
}

/// Wait for every queued member of the law to answer.
async fn settled(
    answers: &Answers,
    count: usize,
) -> Vec<(String, Result<String, String>, Vec<String>)> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if answers.lock().expect("answers").len() == count {
                return answers.lock().expect("answers").clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("every member answers")
}

/// BATCH-ATOMIC-OUTPUT: writes queued together commit in one `COMMIT`, in
/// queue order, and none of them answers before that commit has returned:
/// when each answer is sent, an independent connection already reads every
/// member's row. Under `synchronous=FULL` SQLite makes a commit visible to
/// other connections only after its WAL sync (FIG-5951 verify §9), so no
/// answer leaves before the shared sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_answers_no_member_before_its_shared_commit_returns() {
    let world = world(32, 4_096).await;
    let answers = Answers::default();
    let release = world.hold().await;
    let before = world.commits.load(Ordering::SeqCst);
    for member in ["first", "second", "third"] {
        world.queue(
            move |tx| insert(tx, member).map(|()| TxOutcome::Commit(member)),
            reply(&answers, &world.outside, member),
        );
    }
    drop(release);
    let answered = settled(&answers, 3).await;
    let all = vec!["first".to_owned(), "second".to_owned(), "third".to_owned()];
    for (member, result, seen) in &answered {
        assert!(result.is_ok(), "{member} answered {result:?}");
        assert_eq!(seen, &all, "{member} answered before the batch committed");
    }
    assert_eq!(world.committed(), all, "members commit in queue order");
    assert_eq!(
        world.commits.load(Ordering::SeqCst) - before,
        2,
        "the hold and the three queued writes take two commits"
    );
}

/// One member's refusal (a rollback outcome) or failure (an error) undoes
/// only that member's writes and answers only that member: its siblings,
/// before and after it, commit in queue order in the same `COMMIT`, and the
/// failing member gets its own typed error back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_refusal_or_failure_leaves_its_siblings_committed_in_order() {
    let world = world(32, 4_096).await;
    let answers = Answers::default();
    let release = world.hold().await;
    let before = world.commits.load(Ordering::SeqCst);
    world.queue(
        |tx| insert(tx, "kept-before").map(|()| TxOutcome::Commit("kept")),
        reply(&answers, &world.outside, "kept-before"),
    );
    world.queue(
        |tx| insert(tx, "refused").map(|()| TxOutcome::Rollback("refused")),
        reply(&answers, &world.outside, "refused"),
    );
    world.queue(
        |tx| {
            insert(tx, "failed")?;
            Err::<TxOutcome<&str>, _>(crate::sqlite_conversion_error(
                lash_core_execution::StoreError::Contended,
            ))
        },
        {
            let answers = Arc::clone(&answers);
            move |result: rusqlite::Result<&str>| {
                let typed = matches!(
                    &result,
                    Err(rusqlite::Error::ToSqlConversionFailure(error))
                        if matches!(
                            error.downcast_ref::<lash_core_execution::StoreError>(),
                            Some(lash_core_execution::StoreError::Contended)
                        )
                );
                answers.lock().expect("answers").push((
                    "failed".to_owned(),
                    if typed {
                        Err("typed".to_owned())
                    } else {
                        Ok(format!("{result:?}"))
                    },
                    Vec::new(),
                ));
            }
        },
    );
    world.queue(
        |tx| insert(tx, "kept-after").map(|()| TxOutcome::Commit("kept")),
        reply(&answers, &world.outside, "kept-after"),
    );
    drop(release);
    let answered = settled(&answers, 4).await;
    let outcome = |name: &str| {
        answered
            .iter()
            .find(|(member, ..)| member == name)
            .map(|(_, result, _)| result.clone())
            .expect("member answered")
    };
    assert_eq!(outcome("kept-before"), Ok("\"kept\"".to_owned()));
    assert_eq!(outcome("refused"), Ok("\"refused\"".to_owned()));
    assert_eq!(
        outcome("failed"),
        Err("typed".to_owned()),
        "the failure keeps its type"
    );
    assert_eq!(outcome("kept-after"), Ok("\"kept\"".to_owned()));
    assert_eq!(
        world.committed(),
        vec!["kept-before".to_owned(), "kept-after".to_owned()],
        "only the refused and failed members' writes are undone"
    );
    assert_eq!(world.commits.load(Ordering::SeqCst) - before, 2);
}

/// A failed outer `COMMIT` fails every member, the ones that refused
/// included, and keeps none of their writes; the writer goes on serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outer_commit_failure_fails_every_member() {
    let world = world(32, 4_096).await;
    let answers = Answers::default();
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let refusing = Arc::clone(&refuse);
    world
        .writer
        .call(move |c| {
            c.commit_hook(Some(move || refusing.load(Ordering::SeqCst)));
            Ok(())
        })
        .await
        .expect("install the refusing commit hook");
    let release = world.hold().await;
    refuse.store(true, Ordering::SeqCst);
    world.queue(
        |tx| insert(tx, "one").map(|()| TxOutcome::Commit(())),
        reply(&answers, &world.outside, "one"),
    );
    world.queue(
        |tx| insert(tx, "refused").map(|()| TxOutcome::Rollback(())),
        reply(&answers, &world.outside, "refused"),
    );
    world.queue(
        |tx| insert(tx, "two").map(|()| TxOutcome::Commit(())),
        reply(&answers, &world.outside, "two"),
    );
    // The hold's own commit is refused too; it answers nobody.
    drop(release);
    let answered = settled(&answers, 3).await;
    for (member, result, _) in &answered {
        assert!(
            result.is_err(),
            "{member} answered {result:?} after a failed commit"
        );
    }
    assert!(
        world.committed().is_empty(),
        "a failed commit keeps no member's writes"
    );
    refuse.store(false, Ordering::SeqCst);
    world
        .writer
        .write(|tx| insert(tx, "after"))
        .await
        .expect("the writer serves the next write");
    assert_eq!(world.committed(), vec!["after".to_owned()]);
}

/// Exhaust the database's pages inside `tx`: the one-row insert keeps no
/// statement journal, so SQLite answers `SQLITE_FULL` by ending the whole
/// transaction, not only the member's savepoint. Capacity is restored before
/// the body returns, so the members after it can commit.
fn exhaust(tx: &FencedTx<'_>) -> rusqlite::Result<String> {
    let pages: u64 = tx.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    tx.query_row(&format!("PRAGMA max_page_count = {pages}"), [], |row| {
        row.get::<_, u64>(0)
    })?;
    let full = insert(tx, &"x".repeat(524_288)).expect_err("SQLite must exhaust its pages");
    let ended = tx.is_autocommit();
    tx.query_row("PRAGMA max_page_count = 2147483646", [], |row| {
        row.get::<_, u64>(0)
    })?;
    Ok(format!("ended={ended} {full}"))
}

/// A member's page exhaustion inside a batch of several (FIG-5968): SQLite
/// ends the whole transaction, so the members before it lose their writes.
/// A member that kept nothing, with no kept writes before it, keeps its own
/// answer, and so does the exhausting member's own refusal; a member whose
/// writes went with the transaction, or whose refusal was decided against
/// them, fails and is never answered committed. The members after it lead
/// the next batch and commit exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_page_exhaustion_answers_its_siblings_by_what_they_kept() {
    let world = world(32, 4_096).await;
    let answers = Answers::default();
    let full = || |tx: &FencedTx<'_>| exhaust(tx).map(TxOutcome::Rollback);
    let release = world.hold().await;
    world.queue(
        |tx| insert(tx, "refused").map(|()| TxOutcome::Rollback("refused")),
        reply(&answers, &world.outside, "refused"),
    );
    world.queue(full(), reply(&answers, &world.outside, "full-first"));
    world.queue(
        |tx| insert(tx, "after-first").map(|()| TxOutcome::Commit("kept")),
        reply(&answers, &world.outside, "after-first"),
    );
    drop(release);
    settled(&answers, 3).await;
    let release = world.hold().await;
    world.queue(
        |tx| insert(tx, "lost").map(|()| TxOutcome::Commit("kept")),
        reply(&answers, &world.outside, "lost"),
    );
    world.queue(full(), reply(&answers, &world.outside, "full-second"));
    world.queue(
        |tx| insert(tx, "after-second").map(|()| TxOutcome::Commit("kept")),
        reply(&answers, &world.outside, "after-second"),
    );
    drop(release);
    let answered = settled(&answers, 6).await;
    let outcome = |name: &str| {
        answered
            .iter()
            .find(|(member, ..)| member == name)
            .map(|(_, result, _)| result.clone())
            .expect("member answered")
    };
    let refusal = Ok("\"ended=true database or disk is full\"".to_owned());
    let ended = |name: &str| {
        outcome(name)
            .is_err_and(|error| error.contains("the write transaction ended inside a batch member"))
    };
    assert_eq!(outcome("refused"), Ok("\"refused\"".to_owned()));
    assert_eq!(outcome("full-first"), refusal, "nothing was kept before it");
    assert_eq!(outcome("after-first"), Ok("\"kept\"".to_owned()));
    assert!(ended("lost"), "its writes went with the transaction");
    assert!(
        ended("full-second"),
        "its refusal was decided against writes that are gone: {:?}",
        outcome("full-second")
    );
    assert_eq!(outcome("after-second"), Ok("\"kept\"".to_owned()));
    assert_eq!(
        world.committed(),
        vec!["after-first".to_owned(), "after-second".to_owned()],
        "the members after the exhaustion commit exactly once"
    );
}

/// BATCH-BOUNDS: a batch holds at most `write_batch_members` writes, and no
/// write starts in a batch that already changed `write_batch_rows` rows; the
/// writes past either bound lead the next batch, in queue order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_bounds_cap_members_and_rows() {
    for (members, rows, per_member, queued, expected) in [
        // Two members per batch.
        (2, 4_096, 1, 5, vec![2, 2, 1]),
        // A batch stops admitting once it changed four rows: the second
        // member starts at three rows and takes the batch to six.
        (32, 4, 3, 5, vec![2, 2, 1]),
    ] {
        let world = world(members, rows).await;
        let answers = Answers::default();
        let batches = Arc::new(Mutex::new(Vec::<(usize, i64)>::new()));
        let release = world.hold().await;
        for index in 0..queued {
            let commits = Arc::clone(&world.commits);
            let batches = Arc::clone(&batches);
            let name = format!("member-{index}");
            world.queue(
                move |tx| {
                    let started_at: i64 =
                        tx.query_row("SELECT COUNT(*) FROM rows", [], |row| row.get(0))?;
                    batches
                        .lock()
                        .expect("batches")
                        .push((commits.load(Ordering::SeqCst), started_at));
                    for _ in 0..per_member {
                        insert(tx, &name)?;
                    }
                    Ok(TxOutcome::Commit(()))
                },
                reply(&answers, &world.outside, &format!("member-{index}")),
            );
        }
        drop(release);
        settled(&answers, queued).await;
        let batches = batches.lock().expect("batches").clone();
        let mut sizes: Vec<usize> = Vec::new();
        let mut last = None;
        for (commit, _) in &batches {
            if last == Some(*commit) {
                *sizes.last_mut().expect("a batch") += 1;
            } else {
                sizes.push(1);
                last = Some(*commit);
            }
        }
        assert_eq!(sizes, expected, "members {members}, rows {rows}");
        let mut first_started = None;
        let mut last = None;
        for (commit, started_at) in &batches {
            if last != Some(*commit) {
                last = Some(*commit);
                first_started = Some(*started_at);
            } else {
                let changed = started_at - first_started.expect("a batch");
                assert!(
                    changed < rows as i64,
                    "a member started after its batch changed {changed} rows"
                );
            }
        }
        assert!(sizes.iter().all(|size| *size <= members));
        let committed = world.committed();
        assert_eq!(committed.len(), queued * per_member);
        let order: Vec<_> = committed.iter().step_by(per_member).cloned().collect();
        let queued_order: Vec<_> = (0..queued).map(|index| format!("member-{index}")).collect();
        assert_eq!(order, queued_order, "batches keep queue order");
    }
}

/// A lone write pays nothing for grouping: with nothing queued behind it,
/// it runs as the plain transaction it always was, with no savepoint and
/// no wait for company, and answers at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lone_write_is_a_plain_transaction_with_no_wait() {
    use lash_core_execution::facade_support::sql;
    let world = world(32, 4_096).await;
    let started = std::time::Instant::now();
    let (answer, receipt) = sql::collect(
        "lone-write",
        "sqlite",
        world.writer.write(|tx| insert(tx, "lone")),
    )
    .await;
    answer.expect("the lone write commits");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "a lone write waited for company"
    );
    let shapes: Vec<_> = receipt.shapes.keys().cloned().collect();
    assert!(
        shapes.iter().all(|shape| !shape.contains("savepoint")),
        "a lone write took a savepoint: {shapes:?}"
    );
    assert!(
        shapes
            .iter()
            .any(|shape| shape.starts_with("begin immediate")),
        "{shapes:?}"
    );
    assert!(shapes.iter().any(|shape| shape == "commit"), "{shapes:?}");

    // The same write grouped behind another does take one: the probe sees
    // savepoints when they happen.
    let release = world.hold().await;
    let answers = Answers::default();
    let grouped = sql::Window::new("grouped-write", "sqlite");
    let second = Arc::clone(&answers);
    let outside = Arc::clone(&world.outside);
    sql::Window::on_worker(Some(grouped.clone()), || {
        world.queue(
            |tx| insert(tx, "a").map(|()| TxOutcome::Commit(())),
            reply(&answers, &outside, "a"),
        );
        world.queue(
            |tx| insert(tx, "b").map(|()| TxOutcome::Commit(())),
            reply(&second, &outside, "b"),
        );
    });
    drop(release);
    settled(&answers, 2).await;
    assert!(
        grouped
            .snapshot()
            .shapes
            .keys()
            .any(|shape| shape.contains("savepoint")),
        "grouped writes take savepoints"
    );
}
