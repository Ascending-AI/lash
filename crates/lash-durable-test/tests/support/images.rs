//! The 1.0 decode-and-resume fixtures' store images (ADR 0106 §1; L11,
//! FIG-5187): a SQLite store a build left behind at a committed phase,
//! committed as its database file and its write-ahead log, each gzipped,
//! under `tests/fixtures/formats/<name>/`.
//!
//! Each image's generator (`#[ignore = "regenerates ..."]`) re-records it
//! with `LASH_REGENERATE=1` into `BUILD_WORKSPACE_DIRECTORY`, as
//! `scripts/release_reset.py` runs every fixture generator.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use lash_durable_test::SimClock;

/// One committed store image.
pub struct Image {
    /// The fixture's name: its files are `<name>/db.gz` and
    /// `<name>/db-wal.gz`.
    pub name: &'static str,
    pub db: &'static [u8],
    pub wal: &'static [u8],
}

/// V0's turn, cut by a crash right after its cell's `Once` operation's
/// outcome committed (`vertical_crash_proof.rs`).
pub const SESSION: Image = Image {
    name: "session",
    db: include_bytes!("../fixtures/formats/session/db.gz"),
    wal: include_bytes!("../fixtures/formats/session/db-wal.gz"),
};

/// A lash_vm process parked on its first sleep, released `ready` by a
/// draining node (`format_fixtures.rs`).
pub const LASH_VM: Image = Image {
    name: "lashvm",
    db: include_bytes!("../fixtures/formats/lashvm/db.gz"),
    wal: include_bytes!("../fixtures/formats/lashvm/db-wal.gz"),
};

/// Every committed image.
pub const ALL: [&Image; 2] = [&SESSION, &LASH_VM];

/// The database file a store set over a fresh directory opens at.
pub fn database_path(dir: &Path) -> std::path::PathBuf {
    dir.join("lash.db")
}

/// A copy of `image` in a fresh directory, opened on `clock`. The directory
/// must outlive the store set.
pub async fn open(
    image: &Image,
    clock: Arc<SimClock>,
) -> (lash_sqlite_store::SqliteStoreSet, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = database_path(dir.path());
    std::fs::write(&path, inflate(image.db)).expect("the image's database is written");
    std::fs::write(path.with_extension("db-wal"), inflate(image.wal))
        .expect("the image's log is written");
    let stores = crate::sim::file(&path, clock).await;
    (stores, dir)
}

/// Record the store at `path` as fixture `name` in the regeneration
/// workspace. Nothing writes to it any more: the log is copied before the
/// database, so a checkpoint between the two copies leaves pages the log
/// replays to the same content.
pub fn regenerate(path: &Path, name: &str) {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let out = std::path::PathBuf::from(
        std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("regeneration workspace"),
    )
    .join("crates/lash-durable-test/tests/fixtures/formats")
    .join(name);
    std::fs::create_dir_all(&out).expect("the fixture directory exists");
    let wal = std::fs::read(path.with_extension("db-wal")).unwrap_or_default();
    let db = std::fs::read(path).expect("the store's database reads");
    std::fs::write(out.join("db-wal.gz"), deflate(&wal)).expect("the log is recorded");
    std::fs::write(out.join("db.gz"), deflate(&db)).expect("the database is recorded");
}

fn inflate(bytes: &[u8]) -> Vec<u8> {
    use std::io::Read as _;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .expect("a committed image inflates");
    out
}

fn deflate(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(bytes).expect("an image deflates");
    encoder.finish().expect("an image deflates")
}
