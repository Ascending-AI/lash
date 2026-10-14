//! The activation's private staging spool (S5, `D-SPOOL`): sealed pages on
//! the parent's local disk until a publication settles them.
//!
//! A spool is content addressed: each staged page is one file named by its
//! digest, and the directory is the index, so the parent's memory holds the
//! spool's usage and nothing per page. Writes stream straight to disk
//! through a rename, so a buffer holds at most the page being written. A
//! quota bounds the bytes staged.
//!
//! A candidate is built at a park: a walk of the candidate root's staged
//! DAG that stops at committed references, marking each page it reaches in
//! a marker directory bucketed by the digest's first byte. Staged pages the
//! walk does not reach are dropped. The manifest then streams the markers
//! in ascending digest order one bucket at a time, which is the store's
//! blob lock order, and can be read again from any cursor.
//!
//! A spool lives exactly as long as its stage lease: settled and deleted
//! on an authoritative commit, discarded with the activation on an
//! authoritative refusal, kept while a publication is ambiguous. It never
//! survives its parent process: a parent that opens its [`SpoolRoot`] reaps
//! the spools of dead parents.
#![expect(
    clippy::disallowed_methods,
    reason = "the spool is the host-supplied staging directory's sole reader and writer (D-SPOOL)"
)]

use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core_store::checkpoint_graph::stage::{
    CandidateManifest, ManifestEntry, ManifestSummary, StageFault, StageLeaseId, StageSpool,
    StageUsage,
};
use lash_core_store::checkpoint_graph::{
    CommitId, DecodeLimits, EncodedNode, NodeId, check_envelope, parse_envelope,
};

/// The name of the file each process directory holds locked while its
/// parent lives.
const LOCK: &str = "lock";

fn io(error: &std::io::Error) -> StageFault {
    StageFault::Io(error.to_string())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// One parent process's staging directory: every spool of every activation
/// it runs lives under it. Its lock file is held for the process's life, so
/// a later parent can tell a dead parent's spools from a live one's.
#[derive(Debug)]
pub struct SpoolRoot {
    dir: PathBuf,
    quota: u64,
    /// Held locked until the process ends.
    _lock: fs::File,
}

impl SpoolRoot {
    /// Opens this process's directory under `parent`, a directory the host
    /// gives its parents for staging, with a quota of `quota` bytes per
    /// spool. The spools of parents that died are deleted first: a spool
    /// never survives its parent, and their activations resume from their
    /// committed roots.
    ///
    /// # Errors
    ///
    /// [`StageFault::Io`] when the directory cannot be prepared.
    pub fn open(parent: &Path, quota: u64) -> Result<Self, StageFault> {
        fs::create_dir_all(parent).map_err(|error| io(&error))?;
        for entry in fs::read_dir(parent).map_err(|error| io(&error))? {
            let dir = entry.map_err(|error| io(&error))?.path();
            let Ok(lock) = fs::OpenOptions::new().write(true).open(dir.join(LOCK)) else {
                continue;
            };
            // A lock nobody holds is a dead parent's.
            if lock.try_lock().is_ok() {
                fs::remove_dir_all(&dir).map_err(|error| io(&error))?;
            }
        }
        let mut nonce = std::collections::hash_map::RandomState::new().build_hasher();
        nonce.write_u32(std::process::id());
        let dir = parent.join(format!("{}-{:016x}", std::process::id(), nonce.finish()));
        fs::create_dir(&dir).map_err(|error| io(&error))?;
        let lock = fs::File::create(dir.join(LOCK)).map_err(|error| io(&error))?;
        lock.lock().map_err(|error| io(&error))?;
        Ok(Self {
            dir,
            quota,
            _lock: lock,
        })
    }

    /// The spool of the activation `lease` names, empty.
    ///
    /// # Errors
    ///
    /// [`StageFault::Io`], also when the lease already has a spool.
    pub fn spool(&self, lease: StageLeaseId) -> Result<DiskSpool, StageFault> {
        let dir = self.dir.join(format!("{:016x}", lease.0));
        fs::create_dir(&dir).map_err(|error| io(&error))?;
        fs::create_dir(dir.join("pages")).map_err(|error| io(&error))?;
        Ok(DiskSpool {
            lease,
            dir,
            usage: StageUsage {
                pages: 0,
                bytes: 0,
                quota_bytes: self.quota,
            },
            candidate: None,
        })
    }
}

/// A candidate the spool holds until its publication settles.
#[derive(Debug)]
struct Candidate {
    summary: ManifestSummary,
}

/// One activation's spool on disk.
#[derive(Debug)]
pub struct DiskSpool {
    lease: StageLeaseId,
    dir: PathBuf,
    usage: StageUsage,
    candidate: Option<Candidate>,
}

impl DiskSpool {
    fn pages(&self) -> PathBuf {
        self.dir.join("pages")
    }

    fn page(&self, id: &NodeId) -> PathBuf {
        self.pages().join(id.to_hex())
    }

    fn staged(&self, id: &NodeId) -> bool {
        self.page(id).is_file()
    }

    fn markers(&self, commit: CommitId) -> PathBuf {
        self.dir.join(format!("candidate-{}", hex(&commit.0)))
    }

    /// The candidate committed under `commit`, if it is the one this spool
    /// holds: its manifest, read again from the start.
    fn manifest(&self, commit: CommitId) -> Option<Box<dyn CandidateManifest>> {
        let candidate = self.candidate.as_ref()?;
        (candidate.summary.commit == commit).then(|| {
            Box::new(DiskManifest {
                pages: self.pages(),
                markers: self.markers(commit),
                summary: candidate.summary,
            }) as Box<dyn CandidateManifest>
        })
    }

    /// Deletes every staged page and candidate marker, keeping the spool.
    fn clear(&mut self) -> Result<(), StageFault> {
        for entry in fs::read_dir(&self.dir).map_err(|error| io(&error))? {
            let path = entry.map_err(|error| io(&error))?.path();
            fs::remove_dir_all(&path).map_err(|error| io(&error))?;
        }
        fs::create_dir(self.pages()).map_err(|error| io(&error))?;
        self.usage.pages = 0;
        self.usage.bytes = 0;
        self.candidate = None;
        Ok(())
    }
}

impl Drop for DiskSpool {
    fn drop(&mut self) {
        // Best effort: an activation that ends drops its spool. A parent
        // that dies leaves it to the next parent's `SpoolRoot::open`.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl StageSpool for DiskSpool {
    fn lease(&self) -> StageLeaseId {
        self.lease
    }

    fn seal(
        &mut self,
        page: EncodedNode,
        protected: &dyn Fn(&NodeId) -> bool,
    ) -> Result<(), StageFault> {
        let envelope = check_envelope(&page.bytes, page.id, &DecodeLimits::within_total(u64::MAX))
            .map_err(|_| StageFault::HashMismatch(page.id))?;
        if self.staged(&page.id) {
            return Ok(());
        }
        if let Some(child) = envelope
            .children
            .iter()
            .find(|child| !protected(child) && !self.staged(child))
        {
            return Err(StageFault::Unauthorized {
                page: page.id,
                child,
            });
        }
        let len = page.bytes.len() as u64;
        if self.usage.bytes.saturating_add(len) > self.usage.quota_bytes {
            return Err(StageFault::Quota {
                quota: self.usage.quota_bytes,
            });
        }
        let target = self.page(&page.id);
        let partial = self.pages().join(format!(".partial-{}", page.id.to_hex()));
        fs::write(&partial, &page.bytes).map_err(|error| io(&error))?;
        fs::rename(&partial, &target).map_err(|error| io(&error))?;
        self.usage.pages += 1;
        self.usage.bytes += len;
        Ok(())
    }

    fn read(&self, page: &NodeId) -> Result<Arc<[u8]>, StageFault> {
        match fs::read(self.page(page)) {
            Ok(bytes) => Ok(bytes.into()),
            Err(error) if error.kind() == ErrorKind::NotFound => Err(StageFault::Missing(*page)),
            Err(error) => Err(io(&error)),
        }
    }

    fn candidate(
        &mut self,
        root: NodeId,
        base: Option<NodeId>,
        commit: CommitId,
    ) -> Result<Box<dyn CandidateManifest>, StageFault> {
        if let Some(candidate) = &self.candidate {
            // The exact candidate again, as a retry after an ambiguous
            // publication reads it.
            if candidate.summary.root == root
                && candidate.summary.base == base
                && let Some(manifest) = self.manifest(commit)
            {
                return Ok(manifest);
            }
            return Err(StageFault::Io(format!(
                "a candidate under commit {} is still unresolved",
                hex(&candidate.summary.commit.0)
            )));
        }
        if !self.staged(&root) {
            return Err(StageFault::Missing(root));
        }
        let markers = self.markers(commit);
        let marker = |id: &NodeId, fresh: bool| {
            markers.join(hex(&id.0[..1])).join(format!(
                "{}.{}",
                id.to_hex(),
                if fresh { 'f' } else { 'e' }
            ))
        };
        let mark = |id: &NodeId, fresh: bool| -> Result<bool, StageFault> {
            let path = marker(id, fresh);
            if path.is_file() {
                return Ok(false);
            }
            if let Some(bucket) = path.parent() {
                fs::create_dir_all(bucket).map_err(|error| io(&error))?;
            }
            fs::write(&path, []).map_err(|error| io(&error))?;
            Ok(true)
        };
        let mut summary = ManifestSummary {
            root,
            base,
            commit,
            fresh_nodes: 0,
            fresh_bytes: 0,
            fresh_edges: 0,
            external: 0,
        };
        // Staged pages name only committed pages or pages staged before
        // them, so the walk is over a DAG and ends; the stack holds digests
        // still to visit, never page bytes.
        mark(&root, true)?;
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let bytes = self.read(&id)?;
            let envelope =
                parse_envelope(&bytes, Some(id)).map_err(|_| StageFault::HashMismatch(id))?;
            summary.fresh_nodes += 1;
            summary.fresh_bytes += bytes.len() as u64;
            summary.fresh_edges += envelope.children.len() as u64;
            for child in envelope.children.iter() {
                if self.staged(&child) {
                    if mark(&child, true)? {
                        stack.push(child);
                    }
                } else if mark(&child, false)? {
                    summary.external += 1;
                }
            }
        }
        // Drop what the candidate does not reach.
        for entry in fs::read_dir(self.pages()).map_err(|error| io(&error))? {
            let entry = entry.map_err(|error| io(&error))?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(NodeId::from_hex) else {
                continue;
            };
            if !marker(&id, true).is_file() {
                let len = entry.metadata().map_err(|error| io(&error))?.len();
                fs::remove_file(entry.path()).map_err(|error| io(&error))?;
                self.usage.pages -= 1;
                self.usage.bytes -= len;
            }
        }
        self.candidate = Some(Candidate { summary });
        self.manifest(commit)
            .ok_or_else(|| StageFault::Io("the candidate was not kept".to_owned()))
    }

    fn settle(&mut self, commit: CommitId) -> Result<(), StageFault> {
        match &self.candidate {
            Some(candidate) if candidate.summary.commit == commit => self.clear(),
            _ => Err(StageFault::Io(format!(
                "no candidate under commit {} is held",
                hex(&commit.0)
            ))),
        }
    }

    fn discard(self: Box<Self>) {
        drop(self);
    }

    fn usage(&self) -> StageUsage {
        self.usage
    }
}

/// A candidate's manifest over its marker directory.
#[derive(Debug)]
struct DiskManifest {
    pages: PathBuf,
    markers: PathBuf,
    summary: ManifestSummary,
}

impl CandidateManifest for DiskManifest {
    fn summary(&self) -> ManifestSummary {
        self.summary
    }

    fn entries(
        &mut self,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<ManifestEntry>, StageFault> {
        if !self.markers.is_dir() {
            // Settled or discarded under the reader.
            return Err(StageFault::Ended);
        }
        let mut entries = Vec::new();
        let first = after.map_or(0, |after| after.0[0]);
        for bucket in first..=u8::MAX {
            if entries.len() >= limit {
                break;
            }
            let dir = self.markers.join(hex(&[bucket]));
            let listing = match fs::read_dir(&dir) {
                Ok(listing) => listing,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(io(&error)),
            };
            // One bucket's markers: a 256th of the candidate's digests.
            let mut marked = Vec::new();
            for entry in listing {
                let name = entry.map_err(|error| io(&error))?.file_name();
                let Some((digest, tag)) = name.to_str().and_then(|name| name.split_once('.'))
                else {
                    continue;
                };
                let Some(id) = NodeId::from_hex(digest) else {
                    continue;
                };
                if after.is_none_or(|after| id > after) {
                    marked.push((id, tag == "f"));
                }
            }
            marked.sort_unstable();
            for (id, fresh) in marked {
                if entries.len() >= limit {
                    break;
                }
                entries.push(if fresh {
                    let bytes = match fs::read(self.pages.join(id.to_hex())) {
                        Ok(bytes) => bytes,
                        Err(error) if error.kind() == ErrorKind::NotFound => {
                            return Err(StageFault::Missing(id));
                        }
                        Err(error) => return Err(io(&error)),
                    };
                    ManifestEntry::Fresh(EncodedNode {
                        id,
                        bytes: bytes.into(),
                    })
                } else {
                    ManifestEntry::External(id)
                });
            }
        }
        Ok(entries)
    }
}
