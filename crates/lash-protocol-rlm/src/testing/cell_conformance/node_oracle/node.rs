//! The live Node oracle: `oracle.mjs` as a child process.
//!
//! Only the deliberate steps ask Node live: writing `generated/`, the
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

/// Every `*.<extension>` file under `directory`, recursively, as
/// `(shard, text)`: the shard is the path under the directory without the
/// suffix (`generated/sessions/7.json` is shard `7` under `sessions/`), and
/// the list is sorted by it.
#[allow(clippy::disallowed_methods)] // FIG-2971: a corpus check is a host; the checked-in shards are a test's data.
pub(super) fn shard_files(directory: &std::path::Path, extension: &str) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|suffix| suffix == extension) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let shard = path
                .strip_prefix(directory)
                .expect("a file under its directory")
                .with_extension("")
                .to_str()
                .expect("UTF-8 shard name")
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            (shard, text)
        })
        .collect()
}

/// ICU's default collation as Node's `localeCompare` orders the record
/// alphabet (`[A-Za-z0-9._-]`): punctuation before digits before letters at
/// the primary level, letters case-folded, then case (lower before upper) to
/// break ties. The pre-shard `census.tsv` held this order and the session
/// generator's draws index into it, so the sharded census's readers sort the
/// union back into it (FIG-3727).
pub(super) fn collation_key(name: &str) -> (Vec<u16>, Vec<u16>) {
    let primary = |character: char| match character {
        '_' => 1,
        '-' => 2,
        '.' => 3,
        '0'..='9' => 10 + u16::from(character as u8 - b'0'),
        'a'..='z' | 'A'..='Z' => 20 + u16::from(character.to_ascii_lowercase() as u8 - b'a'),
        other => panic!("record key `{name}` carries {other:?}, which the collation cannot order"),
    };
    (
        name.chars().map(primary).collect(),
        name.chars()
            .map(|character| u16::from(character.is_uppercase()))
            .collect(),
    )
}

/// The Test262 census's rows, the union of the `census/<kind>/<name>.tsv`
/// shards in collation order — the order the pre-shard `census.tsv` held.
pub(super) fn census_rows() -> Vec<Vec<String>> {
    let directory = repository_root().join("crates/lash-typescript/tests/test262/census");
    let mut rows = shard_files(&directory, "tsv")
        .into_iter()
        .flat_map(|(shard, text)| {
            text.lines()
                .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
                .map(|line| line.split('\t').map(str::to_owned).collect::<Vec<_>>())
                .map(move |fields| {
                    assert_eq!(
                        fields.len(),
                        5,
                        "census/{shard}.tsv has a malformed row {fields:?}"
                    );
                    fields
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        collation_key(&left[0])
            .cmp(&collation_key(&right[0]))
            .then_with(|| collation_key(&left[1]).cmp(&collation_key(&right[1])))
    });
    rows
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
    /// `~/.local/share/mise/installs/node/<pinned>/bin/node` when present,
    /// else a failure naming both (FIG-3812 — `kiln run`'s Bazel environment
    /// has no `node` on `PATH`).
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
        panic!(
            "the Node session oracle needs Node {}: set LASH_NODE to its binary or install it at {}",
            super::PINNED_NODE,
            mise.display()
        );
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
                    "start the Node session oracle ({} {}): {error}",
                    node.display(),
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
            "the Node oracle exited without answering (LASH_NODE or the mise-installed Node {} — see node_program)",
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
