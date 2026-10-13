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
    pub name: std::borrow::Cow<'static, str>,
    pub db: std::borrow::Cow<'static, [u8]>,
    pub wal: std::borrow::Cow<'static, [u8]>,
}

/// V0's turn, cut by a crash right after its cell's `Once` operation's
/// outcome committed (`vertical_crash_proof.rs`).
pub const SESSION: Image = Image {
    name: std::borrow::Cow::Borrowed("session"),
    db: std::borrow::Cow::Borrowed(include_bytes!("../fixtures/formats/session/db.gz")),
    wal: std::borrow::Cow::Borrowed(include_bytes!("../fixtures/formats/session/db-wal.gz")),
};

/// A kernel process parked on its first sleep, left waiting by a draining
/// node and woken `ready` (`format_fixtures.rs`).
pub const LASH_VM: Image = Image {
    name: std::borrow::Cow::Borrowed("lashvm"),
    db: std::borrow::Cow::Borrowed(include_bytes!("../fixtures/formats/lashvm/db.gz")),
    wal: std::borrow::Cow::Borrowed(include_bytes!("../fixtures/formats/lashvm/db-wal.gz")),
};

/// The original 1.0 images, whose format sets the baseline coverage law checks.
pub const BASELINE: [&Image; 2] = [&SESSION, &LASH_VM];

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
    std::fs::write(&path, inflate(&image.db)).expect("the image's database is written");
    std::fs::write(path.with_extension("db-wal"), inflate(&image.wal))
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

/// The retained archive uses the same gzip store images as the 1.0 recorder.
/// Base64 keeps every image and its oracle together in one compile-time input.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageSet {
    pub build: String,
    pub kernel_semantics: u32,
    pub helper_releases: serde_json::Value,
    pub images: Vec<RetainedImage>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedImage {
    pub name: String,
    pub kind: String,
    pub source: String,
    pub expected: serde_json::Value,
    pub db_gzip_base64: String,
    pub wal_gzip_base64: String,
}

pub fn retained() -> &'static [ImageSet] {
    static CORPUS: std::sync::OnceLock<Vec<ImageSet>> = std::sync::OnceLock::new();
    CORPUS.get_or_init(|| {
        serde_json::from_str(include_str!("../fixtures/formats/corpus.json"))
            .expect("the retained image corpus decodes")
    })
}

impl RetainedImage {
    pub fn image(&self) -> Image {
        use base64::Engine as _;
        let decode = |encoded: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("the retained image's gzip bytes decode")
        };
        Image {
            name: self.name.clone().into(),
            db: decode(&self.db_gzip_base64).into(),
            wal: decode(&self.wal_gzip_base64).into(),
        }
    }
}

/// Capture via the existing recorder's encoding, without changing an old image.
pub fn capture(path: &Path, name: &str, kind: &str, source: &str) -> RetainedImage {
    use base64::Engine as _;
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(deflate(bytes));
    RetainedImage {
        name: name.to_owned(),
        kind: kind.to_owned(),
        source: source.to_owned(),
        expected: serde_json::Value::Null,
        wal_gzip_base64: encode(&std::fs::read(path.with_extension("db-wal")).unwrap_or_default()),
        db_gzip_base64: encode(&std::fs::read(path).expect("the database reads")),
    }
}

/// Append a complete build's witnesses. Existing records are never replaced.
pub fn append(set: ImageSet) {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let path = std::path::PathBuf::from(
        std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("the recording workspace"),
    )
    .join("crates/lash-durable-test/tests/fixtures/formats/corpus.json");
    let mut sets: Vec<ImageSet> = serde_json::from_slice(&std::fs::read(&path).unwrap())
        .expect("the existing archive decodes");
    assert!(
        !sets.iter().any(|old| old.build == set.build),
        "this build already has retained witnesses"
    );
    assert!(!set.images.is_empty());
    sets.push(set);
    std::fs::write(path, serde_json::to_string_pretty(&sets).unwrap() + "\n")
        .expect("the new image set appends");
}
