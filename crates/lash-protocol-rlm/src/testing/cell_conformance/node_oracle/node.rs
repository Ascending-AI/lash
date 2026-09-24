//! The live Node oracle: `oracle.mjs` as a child process.
//!
//! Only the deliberate steps ask Node live: writing `generated.json`, the
//! long generated run and the minimizer. The cacheable test partition reads
//! the checked-in answers and never starts a process (ADR 0062: no network,
//! no Node in the Bazel test partition).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::Serialize;

use super::Observation;

/// The repository checkout: the workspace `kiln run` (Bazel) names, or the
/// one Cargo compiled from.
#[allow(clippy::disallowed_methods)] // FIG-2971: a test is a host; the live Node oracle is a test host capability.
pub(super) fn repository_root() -> PathBuf {
    std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        PathBuf::from,
    )
}

/// The directory both session corpora live in.
pub(super) fn sessions_directory() -> PathBuf {
    repository_root().join("crates/lash-typescript/tests/differential/sessions")
}

/// One session as the oracle service reads it.
#[derive(Serialize)]
pub(super) struct NodeSession<'a> {
    pub(super) probe: &'a [String],
    pub(super) cells: Vec<NodeCell<'a>>,
}

#[derive(Serialize)]
pub(super) struct NodeCell<'a> {
    pub(super) source: &'a str,
    /// Set for a cell the dialect rejects statically: it never enters the
    /// realm. A generated cell is never one.
    pub(super) reject: Option<&'a str>,
}

/// A running `oracle.mjs`: one realm per session it is asked about.
pub(super) struct NodeOracle {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl NodeOracle {
    /// Starts the oracle under `LASH_NODE`, or the `node` on `PATH`. The
    /// service itself refuses any Node other than the pinned one.
    #[allow(clippy::disallowed_methods)] // FIG-2971: a test is a host; the live Node oracle is a test host capability.
    pub(super) fn start() -> Self {
        let node = std::env::var_os("LASH_NODE").unwrap_or_else(|| "node".into());
        let script = sessions_directory().join("oracle.mjs");
        let mut child = Command::new(&node)
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|error| {
                panic!(
                    "start the Node session oracle ({} {}): {error}",
                    node.to_string_lossy(),
                    script.display()
                )
            });
        let stdin = child.stdin.take().expect("the oracle's stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("the oracle's stdout is piped"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Node's observation of every cell of `session`, in a fresh realm.
    pub(super) fn observe(&mut self, session: &NodeSession<'_>) -> Vec<Observation> {
        let line = serde_json::to_string(session).expect("a session serializes");
        writeln!(self.stdin, "{line}")
            .and_then(|()| self.stdin.flush())
            .expect("write a session to the Node oracle");
        let mut answer = String::new();
        let read = self
            .stdout
            .read_line(&mut answer)
            .expect("read the Node oracle's answer");
        assert!(
            read > 0,
            "the Node oracle exited without answering (is Node {} on PATH or in LASH_NODE?)",
            super::PINNED_NODE
        );
        serde_json::from_str(&answer).expect("the Node oracle answers in the observation shape")
    }
}

impl Drop for NodeOracle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
