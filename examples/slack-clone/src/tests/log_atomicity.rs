//! Pins the per-line atomicity of [`crate::log`].
//!
//! FIG-1554: the bot's concurrent tasks logged through `println!`/`eprintln!`,
//! which format straight into an unbuffered handle one fragment at a time. A
//! second task writing between two fragments severed the first line, and the
//! full-host E2E judge — which reads the merged `stdout`+`stderr` stream one
//! line at a time — could no longer match it.
//!
//! Four guards. [`concurrent_emits_never_sever_a_line`] drives
//! [`crate::log::emit`] — the same composition `log_out!`/`log_err!` run —
//! from many threads onto one destination that accepts writes in bounded
//! chunks, the way a pipe does past `PIPE_BUF` and the way the two standard
//! streams behave against one file; without the process-wide lock those chunks
//! interleave and the test fails. The others pin the single-write property of
//! one line and keep the emit sites wired to the writer at all.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::log;

/// Threads writing at once — enough contention to sever lines reliably when the
/// output lock is removed.
const WRITERS: usize = 12;
/// Lines per writer.
const LINES_PER_WRITER: usize = 60;
/// Payload long enough to outgrow `stdout`'s line buffer and any plausible
/// atomic-write window, which is where a logical line stops being a single
/// physical write of its own accord.
const PAYLOAD_BYTES: usize = 4096;
/// Bytes one `write` call accepts. A pipe past `PIPE_BUF` behaves this way, and
/// so `write_all` becomes a loop that only the output lock keeps indivisible.
const CHUNK_BYTES: usize = 512;

/// A destination shared by every writer that accepts at most [`CHUNK_BYTES`]
/// per call, yielding between chunks.
#[derive(Clone)]
struct ChunkedSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for ChunkedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let taken = buf.len().min(CHUNK_BYTES);
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(&buf[..taken]);
        // Widen the window a lost lock would leave open, so the failure is a
        // deterministic assertion rather than a rare race.
        thread::yield_now();
        Ok(taken)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn expected_lines() -> BTreeSet<String> {
    let payload = "x".repeat(PAYLOAD_BYTES);
    (0..WRITERS)
        .flat_map(|writer| {
            let payload = payload.clone();
            (0..LINES_PER_WRITER)
                .map(move |line| format!("slack-clone-test {writer}-{line} {payload} end"))
        })
        .collect()
}

fn assert_whole_lines(contents: &str) {
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(
        lines.len(),
        WRITERS * LINES_PER_WRITER,
        "every logical line must be exactly one physical line"
    );
    let observed: BTreeSet<String> = lines.iter().map(|line| (*line).to_string()).collect();
    let expected = expected_lines();
    let severed = observed.difference(&expected).count();
    assert!(
        severed == 0 && observed == expected,
        "{severed} of {} physical lines were severed or interleaved",
        lines.len()
    );
}

#[test]
fn concurrent_emits_never_sever_a_line() {
    let sink = ChunkedSink(Arc::new(Mutex::new(Vec::new())));
    let payload = "x".repeat(PAYLOAD_BYTES);
    thread::scope(|scope| {
        for writer in 0..WRITERS {
            let mut sink = sink.clone();
            let payload = payload.as_str();
            scope.spawn(move || {
                for line in 0..LINES_PER_WRITER {
                    // The production composition: render, lock, one write_all.
                    log::emit(
                        format_args!("slack-clone-test {writer}-{line} {payload} end"),
                        |rendered| log::write_atomic(&mut sink, rendered),
                    )
                    .expect("write the log line");
                }
            });
        }
    });

    let bytes = sink.0.lock().expect("read the shared sink").clone();
    assert_whole_lines(&String::from_utf8(bytes).expect("utf-8"));
}

#[test]
fn concurrent_emits_to_one_file_stay_whole() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("merged.log");
    File::create(&path).expect("create the shared log");

    let payload = "x".repeat(PAYLOAD_BYTES);
    thread::scope(|scope| {
        for writer in 0..WRITERS {
            let path: &Path = path.as_path();
            let payload = payload.as_str();
            scope.spawn(move || {
                // One handle per writer onto the same file, exactly as the
                // separate `stdout`/`stderr` handles of a real process share
                // one destination under the E2E harness.
                let mut handle = OpenOptions::new()
                    .append(true)
                    .open(path)
                    .expect("open the shared log for appending");
                for line in 0..LINES_PER_WRITER {
                    log::emit(
                        format_args!("slack-clone-test {writer}-{line} {payload} end"),
                        |rendered| log::write_atomic(&mut handle, rendered),
                    )
                    .expect("write the log line");
                }
            });
        }
    });

    assert_whole_lines(&std::fs::read_to_string(&path).expect("read the shared log"));
}

#[test]
fn every_emit_site_goes_through_the_atomic_writer() {
    // Assembled at runtime so this test's own source does not match itself.
    let banned: Vec<String> = [
        format!("{}!", "print"),
        format!("{}!", "println"),
        format!("{}!", "eprint"),
        format!("{}!", "eprintln"),
        format!("{}!", "dbg"),
        format!("io::{}(", "stdout"),
        format!("io::{}(", "stderr"),
    ]
    .into_iter()
    .collect();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    // `log.rs` owns the atomic writer and this file names the macros to ban;
    // both are exempted by path, so a same-named file elsewhere is still
    // scanned.
    let exempt = [
        manifest.join("src/log.rs"),
        manifest.join("src/tests/log_atomicity.rs"),
    ];

    let mut offenders = Vec::new();
    let mut pending = vec![manifest.join("src"), manifest.join("tests")];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).expect("read the crate source tree") {
            let path = entry.expect("read a source entry").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") || exempt.contains(&path)
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read a source file");
            if banned.iter().any(|needle| source.contains(needle)) {
                offenders.push(
                    path.strip_prefix(manifest)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                );
            }
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "these files write to a standard stream directly; use log_out!/log_err! instead: {offenders:?}"
    );
}

/// Documents the failure mode the atomic writer removes, without asserting on a
/// race: the old path emitted one write per format fragment, so an interleaving
/// writer could land inside a line.
#[test]
fn a_rendered_line_is_a_single_write() {
    struct CountingWriter {
        writes: usize,
        bytes: Vec<u8>,
    }
    impl std::io::Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut writer = CountingWriter {
        writes: 0,
        bytes: Vec::new(),
    };
    log::emit(
        format_args!(
            "slack-clone-bot settled deferred event {}: {:?}",
            "EvMW0Q77S5H", "Replied"
        ),
        |rendered| log::write_atomic(&mut writer, rendered),
    )
    .expect("write the rendered line");

    assert_eq!(
        writer.writes, 1,
        "a log line must reach the fd in one write"
    );
    assert_eq!(
        String::from_utf8(writer.bytes).expect("utf-8"),
        "slack-clone-bot settled deferred event EvMW0Q77S5H: \"Replied\"\n"
    );
}
