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

/// [`LASH_VM`] as the last whole-state build recorded it, before the
/// persistent heap replaced the parked shape (FIG-6193): the refusal
/// witness. Its process's parked run is a KernelWhole root.
pub const PRE_HEAP_LASH_VM: Image = Image {
    name: std::borrow::Cow::Borrowed("pre-heap-lashvm"),
    db: std::borrow::Cow::Borrowed(include_bytes!("../fixtures/formats/pre-heap-lashvm/db.gz")),
    wal: std::borrow::Cow::Borrowed(include_bytes!(
        "../fixtures/formats/pre-heap-lashvm/db-wal.gz"
    )),
};

/// [`SESSION`] as the last whole-state build recorded it (FIG-6193): the
/// refusal witness of a cell parked as a KernelWhole graph root.
pub const PRE_HEAP_SESSION: Image = Image {
    name: std::borrow::Cow::Borrowed("pre-heap-session"),
    db: std::borrow::Cow::Borrowed(include_bytes!("../fixtures/formats/pre-heap-session/db.gz")),
    wal: std::borrow::Cow::Borrowed(include_bytes!(
        "../fixtures/formats/pre-heap-session/db-wal.gz"
    )),
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
    regenerate_at(&recording_root(), path, name);
}

pub fn regenerate_at(root: &Path, path: &Path, name: &str) {
    component(name);
    let out = root.join(name);
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
/// Base64 keeps each image and its oracle together in its own archive file.
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
    CORPUS
        .get_or_init(|| load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats")))
}

fn entries(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("the retained archive directory reads")
        .map(|entry| entry.expect("an archive entry reads").path())
        .collect();
    paths.sort();
    paths
}

pub fn load(root: &Path) -> Vec<ImageSet> {
    let mut sets = Vec::new();
    for build in entries(&root.join("corpus")) {
        let mut grouped: Option<ImageSet> = None;
        for path in entries(&build) {
            let mut entry = read_entry(&path);
            assert_eq!(path, image_path(root, &entry.build, &entry.images[0].name));
            if let Some(set) = &mut grouped {
                assert_eq!(set.build, entry.build);
                assert_eq!(set.kernel_semantics, entry.kernel_semantics);
                assert_eq!(set.helper_releases, entry.helper_releases);
                set.images.append(&mut entry.images);
            } else {
                grouped = Some(entry);
            }
        }
        sets.push(grouped.expect("a retained build has images"));
    }
    sets
}

fn read_entry(path: &Path) -> ImageSet {
    let entry: ImageSet =
        serde_json::from_slice(&std::fs::read(path).expect("the retained image file reads"))
            .expect("the retained image entry decodes");
    assert_eq!(entry.images.len(), 1, "each archive file owns one image");
    entry
}

fn component(name: &str) {
    assert!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "an image path component is a name, not a path: {name}"
    );
}

fn image_path(root: &Path, build: &str, name: &str) -> std::path::PathBuf {
    component(build);
    component(name);
    root.join("corpus").join(build).join(format!("{name}.json"))
}

fn recording_root() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("the recording workspace"),
    )
    .join("crates/lash-durable-test/tests/fixtures/formats")
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
    let root = recording_root();
    assert!(
        !root.join("corpus").join(&set.build).exists(),
        "this build already has retained witnesses"
    );
    append_at(&root, set);
}

pub fn append_at(root: &Path, set: ImageSet) {
    use std::io::Write as _;
    assert!(!set.images.is_empty());
    for image in set.images {
        let path = image_path(root, &set.build, &image.name);
        let entry = ImageSet {
            build: set.build.clone(),
            kernel_semantics: set.kernel_semantics,
            helper_releases: set.helper_releases.clone(),
            images: vec![image],
        };
        std::fs::create_dir_all(path.parent().unwrap()).expect("the build directory exists");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("a new retained image never replaces an existing witness");
        file.write_all((serde_json::to_string_pretty(&entry).unwrap() + "\n").as_bytes())
            .expect("the image entry appends");
    }
}

/// Before the cut, refresh exactly one authorized image, keeping its oracle
/// and recording metadata. Sealed witnesses must never use this recorder.
pub fn rerecord(build: &str, image: RetainedImage) {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    rerecord_at(&recording_root(), build, image);
}

pub fn rerecord_at(root: &Path, build: &str, image: RetainedImage) {
    let path = image_path(root, build, &image.name);
    let mut entry = read_entry(&path);
    assert_eq!(entry.build, build);
    let old = &mut entry.images[0];
    assert_eq!(old.name, image.name);
    assert_eq!(old.kind, image.kind);
    assert_eq!(old.source, image.source);
    assert_eq!(
        old.expected, image.expected,
        "a refresh keeps the recorded result"
    );
    old.db_gzip_base64 = image.db_gzip_base64;
    old.wal_gzip_base64 = image.wal_gzip_base64;
    std::fs::write(path, serde_json::to_string_pretty(&entry).unwrap() + "\n")
        .expect("only the selected image is refreshed");
}
