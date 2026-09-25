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
    /// The pinned Node binary: `LASH_NODE` when set, else the mise install
    /// `~/.local/share/mise/installs/node/<pinned>/bin/node` when present
    /// (FIG-3812: `kiln run`'s Bazel environment has no `node` on `PATH`),
    /// else `node` from `PATH`, which is how CI jobs provision it
    /// (`actions/setup-node`). The oracle refuses any Node other than the
    /// pinned one, so a wrong `PATH` node fails loudly rather than drifting.
    #[allow(clippy::disallowed_methods)] // FIG-2971: a test is a host; the live Node oracle is a test host capability.
    fn node_program() -> PathBuf {
        if let Some(node) = std::env::var_os("LASH_NODE") {
            return PathBuf::from(node);
        }
        let suffix = format!(
            ".local/share/mise/installs/node/{}/bin/node",
            super::PINNED_NODE.trim_start_matches('v')
        );
        let mise = std::env::var_os("HOME").map_or_else(
            || PathBuf::from("~").join(&suffix),
            |home| PathBuf::from(home).join(&suffix),
        );
        if mise.is_file() {
            return mise;
        }
        PathBuf::from("node")
    }

    /// Starts the oracle under [`Self::node_program`]'s resolution. The
    /// service itself refuses any Node other than the pinned one.
    #[allow(clippy::disallowed_methods)] // FIG-2971: a test is a host; the live Node oracle is a test host capability.
    pub(super) fn start() -> Self {
        let node = Self::node_program();
        let script = sessions_directory().join("oracle.mjs");
        let mut child = Command::new(&node)
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|error| {
                panic!(
                    "start the Node session oracle ({} {}): {error}; it needs Node {}: set \
                     LASH_NODE, install it with mise, or put it on PATH",
                    node.display(),
                    script.display(),
                    super::PINNED_NODE
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
            "the Node oracle exited without answering (it needs Node {} from LASH_NODE, mise or PATH — see node_program)",
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
