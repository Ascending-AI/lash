//! The activation's private staging spool (S5, `D-SPOOL`): sealed pages on
//! the parent's local disk until a publication settles them.
//!
//! A spool is content addressed: each staged page is one file named by its
//! digest, and the directory is the index, so the parent's memory holds the
//! spool's usage and nothing per page. Writes stream straight to disk
//! through a rename, so a buffer holds at most the page being written. A
//! quota bounds the spool's file contents: each staged page's bytes, plus
//! [`ENTRY_BYTES`] of manifest scratch for each staged page and each
//! distinct external reference (FIG-6512).
//!
//! A candidate is built at a park: a walk of the candidate root's staged
//! DAG that stops at committed references, marking each page it reaches in
//! a marker directory bucketed by the digest's first byte and appending its
//! record to a file that is also the walk's queue. An external sort with
//! bounded runs orders the records into the manifest file, ascending by
//! digest, which is the store's blob lock order; the manifest streams from
//! any cursor, and settlement streams it again to delete the candidate's
//! pages. No step holds more than a fixed number of records in memory
//! (FIG-6421). Staged pages no candidate reaches stay until the spool's
//! owner names the roots it still needs ([`StageSpool::retain`]), so the
//! cells of one session can share a spool.
//!
//! A spool lives exactly as long as its stage lease: an authoritative
//! commit deletes the candidate's pages, an authoritative refusal discards
//! the spool with the activation, and an ambiguous publication keeps it. It
//! never survives its parent process: a parent that opens its
//! [`SpoolRoot`] reaps the spools of dead parents.
#![expect(
    clippy::disallowed_methods,
    reason = "the spool is the host-supplied staging directory's sole reader and writer (D-SPOOL)"
)]

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// The name of the file in the staging directory a parent holds locked
/// while it reaps and creates its own process directory.
const OPENING: &str = "opening.lock";

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

/// One manifest record: a page's digest, then `f` when the candidate
/// supplies the page or `e` when it references it committed. Records sort
/// by digest.
const RECORD: usize = 33;
type Record = [u8; RECORD];
/// What one staged page or distinct external reference charges the quota
/// beside its page bytes: the manifest scratch its record can occupy at
/// once, in the walk's queue, a sorted run and a merge's output.
pub(crate) const ENTRY_BYTES: u64 = 3 * RECORD as u64;
/// The records one sorted run holds in memory: the sort's one buffer that
/// grows with its input, up to this bound.
const RUN_RECORDS: usize = 4096;
/// The runs one merge reads at once.
const MERGE_FANIN: usize = 64;
/// The buffer a streamed record file reads or writes through.
const RECORD_BUFFER: usize = 8 << 10;

fn record(id: &NodeId, fresh: bool) -> Record {
    let mut record = [0; RECORD];
    record[..32].copy_from_slice(&id.0);
    record[32] = if fresh { b'f' } else { b'e' };
    record
}

fn parse(record: &Record) -> (NodeId, bool) {
    let mut id = [0; 32];
    id.copy_from_slice(&record[..32]);
    (NodeId(id), record[32] == b'f')
}

/// The next record `reader` streams, none at its end.
fn next_record(reader: &mut impl Read) -> Result<Option<Record>, StageFault> {
    let mut record = [0; RECORD];
    match reader.read_exact(&mut record) {
        Ok(()) => Ok(Some(record)),
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => Ok(None),
        Err(error) => Err(io(&error)),
    }
}

/// Writes `record` to `out`, a record written.
fn append(out: &mut impl Write, record: &Record, meter: &SpoolMeter) -> Result<(), StageFault> {
    meter.touch(1);
    out.write_all(record).map_err(|error| io(&error))
}

fn reader(path: &Path) -> Result<BufReader<File>, StageFault> {
    File::open(path)
        .map(|file| BufReader::with_capacity(RECORD_BUFFER, file))
        .map_err(|error| io(&error))
}

fn writer(path: &Path) -> Result<BufWriter<File>, StageFault> {
    File::create(path)
        .map(|file| BufWriter::with_capacity(RECORD_BUFFER, file))
        .map_err(|error| io(&error))
}

/// Sorts the records of `unsorted` into `sorted` (FIG-6421): runs of at
/// most [`RUN_RECORDS`], each sorted in memory, then merge passes of at
/// most [`MERGE_FANIN`] runs each, every run streamed through one buffer.
/// The walk already counted the records, so a small input reserves only
/// its actual run rather than the large-manifest bound (FIG-6458).
fn sort_records(
    dir: &Path,
    unsorted: &Path,
    sorted: &Path,
    records: u64,
    meter: &SpoolMeter,
) -> Result<(), StageFault> {
    let run = |pass: usize, at: usize| dir.join(format!("run-{pass}-{at}"));
    let mut input = reader(unsorted)?;
    let mut buffer: Vec<Record> = Vec::with_capacity(records.min(RUN_RECORDS as u64) as usize);
    let mut runs = 0;
    loop {
        while buffer.len() < RUN_RECORDS
            && let Some(record) = next_record(&mut input)?
        {
            buffer.push(record);
        }
        if buffer.is_empty() {
            break;
        }
        meter.holding((buffer.capacity() * RECORD + 2 * RECORD_BUFFER) as u64);
        meter.touch(buffer.len() as u64);
        buffer.sort_unstable();
        let mut output = writer(&run(0, runs))?;
        for record in buffer.drain(..) {
            append(&mut output, &record, meter)?;
        }
        output.flush().map_err(|error| io(&error))?;
        runs += 1;
    }
    drop(buffer);
    let mut pass = 0;
    while runs > 1 {
        let mut merged = 0;
        for first in (0..runs).step_by(MERGE_FANIN) {
            let inputs: Vec<PathBuf> = (first..runs.min(first + MERGE_FANIN))
                .map(|at| run(pass, at))
                .collect();
            merge(&inputs, &run(pass + 1, merged), meter)?;
            for path in inputs {
                fs::remove_file(path).map_err(|error| io(&error))?;
            }
            merged += 1;
        }
        runs = merged;
        pass += 1;
    }
    fs::rename(run(pass, 0), sorted).map_err(|error| io(&error))
}

/// Merges the sorted runs `inputs` into one sorted run at `output`.
fn merge(inputs: &[PathBuf], output: &Path, meter: &SpoolMeter) -> Result<(), StageFault> {
    let mut readers = inputs
        .iter()
        .map(|path| reader(path))
        .collect::<Result<Vec<_>, _>>()?;
    meter.holding(((inputs.len() + 1) * RECORD_BUFFER + inputs.len() * (RECORD + 8)) as u64);
    let mut heads = BinaryHeap::with_capacity(readers.len());
    for (at, reader) in readers.iter_mut().enumerate() {
        if let Some(record) = next_record(reader)? {
            heads.push(Reverse((record, at)));
        }
    }
    let mut out = writer(output)?;
    while let Some(Reverse((record, at))) = heads.pop() {
        meter.touch(1);
        append(&mut out, &record, meter)?;
        if let Some(next) = next_record(&mut readers[at])? {
            heads.push(Reverse((next, at)));
        }
    }
    out.flush().map_err(|error| io(&error))
}

/// What a spool's candidate metadata cost (FIG-6421): the digest records it
/// wrote and read, and the most record bytes it held in memory at once.
#[derive(Debug, Default)]
pub(crate) struct SpoolMeter {
    touched: AtomicU64,
    peak: AtomicU64,
}

impl SpoolMeter {
    fn touch(&self, records: u64) {
        self.touched.fetch_add(records, Ordering::Relaxed);
    }

    fn holding(&self, bytes: u64) {
        self.peak.fetch_max(bytes, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn touched(&self) -> u64 {
        self.touched.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }
}

/// One parent process's staging directory: every spool of every activation
/// it runs lives under it. Its lock file is held for the process's life, so
/// a later parent can tell a dead parent's spools from a live one's.
#[derive(Debug)]
pub struct SpoolRoot {
    dir: PathBuf,
    quota: u64,
    /// Cumulative successful page writes across this runtime's activations.
    #[cfg(feature = "perf-witness")]
    spooled_bytes: Arc<AtomicU64>,
    /// Held locked until the process ends.
    _lock: fs::File,
}

impl SpoolRoot {
    /// Opens this process's directory under `parent`, a directory the host
    /// gives its parents for staging, with a quota of `quota` bytes per
    /// spool: page bytes plus 99 bytes of manifest scratch for each staged
    /// page and distinct external reference. The spools of parents that
    /// died are deleted first: a spool never survives its parent, and their
    /// activations resume from their committed roots.
    ///
    /// # Errors
    ///
    /// [`StageFault::Io`] when the directory cannot be prepared.
    pub fn open(parent: &Path, quota: u64) -> Result<Self, StageFault> {
        fs::create_dir_all(parent).map_err(|error| io(&error))?;
        // Parents open one at a time: a sibling's directory is reaped only
        // after its open, which holds its lock from the moment the lock
        // file exists, and no two parents reap one directory (FIG-6438).
        let opening = fs::File::create(parent.join(OPENING)).map_err(|error| io(&error))?;
        opening.lock().map_err(|error| io(&error))?;
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
            #[cfg(feature = "perf-witness")]
            spooled_bytes: Arc::default(),
            _lock: lock,
        })
    }

    /// Page bytes this runtime has successfully staged, including pages since
    /// settled or discarded. Re-sealing a page already in a spool adds nothing.
    /// Other roots, even under the same parent directory, have separate counts.
    #[cfg(feature = "perf-witness")]
    pub fn spooled_bytes(&self) -> u64 {
        self.spooled_bytes.load(Ordering::Relaxed)
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
        fs::create_dir(dir.join("refs")).map_err(|error| io(&error))?;
        Ok(DiskSpool {
            lease,
            dir,
            #[cfg(feature = "perf-witness")]
            spooled_bytes: Arc::clone(&self.spooled_bytes),
            usage: StageUsage {
                pages: 0,
                bytes: 0,
                quota_bytes: self.quota,
            },
            candidate: None,
            entries: 0,
            meter: Arc::default(),
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
    #[cfg(feature = "perf-witness")]
    spooled_bytes: Arc<AtomicU64>,
    candidate: Option<Candidate>,
    /// Staged pages plus distinct external references, each charged
    /// [`ENTRY_BYTES`] of the quota.
    entries: u64,
    meter: Arc<SpoolMeter>,
}

impl DiskSpool {
    #[cfg(test)]
    pub(crate) fn meter(&self) -> Arc<SpoolMeter> {
        Arc::clone(&self.meter)
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> u64 {
        self.entries
    }

    fn pages(&self) -> PathBuf {
        self.dir.join("pages")
    }

    fn page(&self, id: &NodeId) -> PathBuf {
        self.pages().join(id.to_hex())
    }

    /// Incoming edges from staged pages, including edges to staged children.
    /// The count is a filename under the child's digest directory. These
    /// empty files add no content bytes beyond the entry's charge, and the
    /// index stays on disk rather than growing a per-entry map in the parent.
    fn references(&self, id: &NodeId) -> Result<u64, StageFault> {
        let dir = self.dir.join("refs").join(id.to_hex());
        let mut names = match fs::read_dir(dir) {
            Ok(names) => names,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(io(&error)),
        };
        let entry = names
            .next()
            .transpose()
            .map_err(|error| io(&error))?
            .ok_or_else(|| StageFault::Io("a staging reference count is missing".to_owned()))?;
        entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
            .ok_or_else(|| StageFault::Io("a staging reference count is invalid".to_owned()))
    }

    fn reference(&mut self, id: &NodeId, add: bool) -> Result<(), StageFault> {
        let old = self.references(id)?;
        let next = if add { old + 1 } else { old - 1 };
        let dir = self.dir.join("refs").join(id.to_hex());
        match (old, next) {
            (0, _) => {
                fs::create_dir(&dir).map_err(|error| io(&error))?;
                fs::write(dir.join(next.to_string()), []).map_err(|error| io(&error))?;
                self.entries += u64::from(!self.staged(id));
            }
            (_, 0) => {
                fs::remove_file(dir.join(old.to_string())).map_err(|error| io(&error))?;
                fs::remove_dir(&dir).map_err(|error| io(&error))?;
                self.entries -= u64::from(!self.staged(id));
            }
            _ => fs::rename(dir.join(old.to_string()), dir.join(next.to_string()))
                .map_err(|error| io(&error))?,
        }
        Ok(())
    }

    fn staged(&self, id: &NodeId) -> bool {
        self.page(id).is_file()
    }

    /// Where the candidate committed under `commit` keeps its manifest
    /// and, while it is built, its walk.
    fn candidate_dir(&self, commit: CommitId) -> PathBuf {
        self.dir.join(format!("candidate-{}", hex(&commit.0)))
    }

    /// The candidate committed under `commit`, if it is the one this spool
    /// holds: its manifest, read again from the start.
    fn manifest(&self, commit: CommitId) -> Option<Box<dyn CandidateManifest>> {
        let candidate = self.candidate.as_ref()?;
        (candidate.summary.commit == commit).then(|| {
            Box::new(DiskManifest {
                pages: self.pages(),
                manifest: self.candidate_dir(commit).join("manifest"),
                summary: candidate.summary,
                meter: Arc::clone(&self.meter),
                cursor: None,
            }) as Box<dyn CandidateManifest>
        })
    }

    /// Deletes the staged page `id`, if staged.
    fn remove(&mut self, id: &NodeId) -> Result<(), StageFault> {
        let path = self.page(id);
        let len = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io(&error)),
        };
        let bytes = self.read(id)?;
        let envelope =
            parse_envelope(&bytes, Some(*id)).map_err(|_| StageFault::HashMismatch(*id))?;
        fs::remove_file(&path).map_err(|error| io(&error))?;
        self.entries -= u64::from(self.references(id)? == 0);
        for child in envelope.children.iter() {
            self.reference(&child, false)?;
        }
        self.usage.pages -= 1;
        self.usage.bytes -= len;
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
        // The canonical child table is distinct. Count the union of staged
        // pages and their direct children before writing anything; an external
        // reference that becomes staged already occupies its entry.
        let own_entry = u64::from(self.references(&page.id)? == 0);
        let mut added = own_entry;
        for child in envelope.children.iter() {
            if !self.staged(&child) && self.references(&child)? == 0 {
                added += 1;
            }
        }
        let len = page.bytes.len() as u64;
        let charged = self.usage.bytes.saturating_add(len).saturating_add(
            self.entries
                .saturating_add(added)
                .saturating_mul(ENTRY_BYTES),
        );
        if charged > self.usage.quota_bytes {
            return Err(StageFault::Quota {
                quota: self.usage.quota_bytes,
            });
        }
        let target = self.page(&page.id);
        let partial = self.pages().join(format!(".partial-{}", page.id.to_hex()));
        fs::write(&partial, &page.bytes).map_err(|error| io(&error))?;
        fs::rename(&partial, &target).map_err(|error| io(&error))?;
        self.entries += own_entry;
        for child in envelope.children.iter() {
            self.reference(&child, true)?;
        }
        #[cfg(feature = "perf-witness")]
        self.spooled_bytes.fetch_add(len, Ordering::Relaxed);
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

    fn contains(&self, page: &NodeId) -> bool {
        self.staged(page)
    }

    fn retain(&mut self, roots: &[NodeId]) -> Result<(), StageFault> {
        // Mark what the roots reach in a scratch directory, then delete
        // every unmarked page; the stack holds digests, never page bytes.
        let reached = self.dir.join("retain");
        match fs::remove_dir_all(&reached) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(io(&error)),
        }
        fs::create_dir(&reached).map_err(|error| io(&error))?;
        let mut stack: Vec<NodeId> = roots.iter().copied().filter(|id| self.staged(id)).collect();
        for id in &stack {
            fs::write(reached.join(id.to_hex()), []).map_err(|error| io(&error))?;
        }
        while let Some(id) = stack.pop() {
            let bytes = self.read(&id)?;
            let envelope =
                parse_envelope(&bytes, Some(id)).map_err(|_| StageFault::HashMismatch(id))?;
            for child in envelope.children.iter() {
                let marker = reached.join(child.to_hex());
                if self.staged(&child) && !marker.is_file() {
                    fs::write(&marker, []).map_err(|error| io(&error))?;
                    stack.push(child);
                }
            }
        }
        let mut dead = Vec::new();
        for entry in fs::read_dir(self.pages()).map_err(|error| io(&error))? {
            let name = entry.map_err(|error| io(&error))?.file_name();
            if let Some(id) = name.to_str().and_then(NodeId::from_hex)
                && !reached.join(id.to_hex()).is_file()
            {
                dead.push(id);
            }
        }
        for id in dead {
            self.remove(&id)?;
        }
        fs::remove_dir_all(&reached).map_err(|error| io(&error))
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
        let dir = self.candidate_dir(commit);
        // A build that failed part way left its marks: start over.
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(io(&error)),
        }
        let marks = dir.join("marks");
        fs::create_dir_all(&marks).map_err(|error| io(&error))?;
        let mark = |id: &NodeId, fresh: bool| -> Result<bool, StageFault> {
            let bucket = marks.join(hex(&id.0[..1]));
            let path = bucket.join(format!("{}.{}", id.to_hex(), if fresh { 'f' } else { 'e' }));
            if path.is_file() {
                return Ok(false);
            }
            fs::create_dir_all(&bucket).map_err(|error| io(&error))?;
            fs::write(&path, []).map_err(|error| io(&error))?;
            self.meter.touch(1);
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
        // them, so the walk is over a DAG and ends. Each page it reaches is
        // marked once and its record appended to `records`, which is also
        // the walk's queue: read back in order through one buffer, so
        // memory holds no frontier and no page bytes but the page at hand.
        let records = dir.join("records");
        let mut queue = writer(&records)?;
        let mut walk = reader(&records)?;
        self.meter.holding(2 * RECORD_BUFFER as u64);
        let (mut written, mut flushed, mut walked) = (0_u64, 0_u64, 0_u64);
        mark(&root, true)?;
        append(&mut queue, &record(&root, true), &self.meter)?;
        written += 1;
        while walked < written {
            if walked == flushed {
                queue.flush().map_err(|error| io(&error))?;
                flushed = written;
            }
            let Some(next) = next_record(&mut walk)? else {
                return Err(StageFault::Io("the walk's queue ended early".to_owned()));
            };
            walked += 1;
            self.meter.touch(1);
            let (id, fresh) = parse(&next);
            if !fresh {
                continue;
            }
            let bytes = self.read(&id)?;
            let envelope =
                parse_envelope(&bytes, Some(id)).map_err(|_| StageFault::HashMismatch(id))?;
            summary.fresh_nodes += 1;
            summary.fresh_bytes += bytes.len() as u64;
            summary.fresh_edges += envelope.children.len() as u64;
            for child in envelope.children.iter() {
                let fresh = self.staged(&child);
                if mark(&child, fresh)? {
                    append(&mut queue, &record(&child, fresh), &self.meter)?;
                    written += 1;
                    summary.external += u64::from(!fresh);
                }
            }
        }
        queue.flush().map_err(|error| io(&error))?;
        drop((queue, walk));
        sort_records(&dir, &records, &dir.join("manifest"), written, &self.meter)?;
        fs::remove_file(&records).map_err(|error| io(&error))?;
        fs::remove_dir_all(&marks).map_err(|error| io(&error))?;
        self.candidate = Some(Candidate { summary });
        self.manifest(commit)
            .ok_or_else(|| StageFault::Io("the candidate was not kept".to_owned()))
    }

    fn settle(&mut self, commit: CommitId) -> Result<(), StageFault> {
        match &self.candidate {
            Some(candidate) if candidate.summary.commit == commit => {
                // The durable root owns the candidate's pages now; pages
                // staged beside it stay for the spool's owner. The manifest
                // streams through one buffer as its pages are deleted.
                let dir = self.candidate_dir(commit);
                let mut manifest = reader(&dir.join("manifest"))?;
                self.meter.holding(RECORD_BUFFER as u64);
                while let Some(next) = next_record(&mut manifest)? {
                    self.meter.touch(1);
                    if let (id, true) = parse(&next) {
                        self.remove(&id)?;
                    }
                }
                fs::remove_dir_all(&dir).map_err(|error| io(&error))?;
                self.candidate = None;
                Ok(())
            }
            _ => Err(StageFault::Io(format!(
                "no candidate under commit {} is held",
                hex(&commit.0)
            ))),
        }
    }

    fn withdraw(&mut self, commit: CommitId) -> Result<(), StageFault> {
        match &self.candidate {
            Some(candidate) if candidate.summary.commit == commit => {
                fs::remove_dir_all(self.candidate_dir(commit)).map_err(|error| io(&error))?;
                self.candidate = None;
                Ok(())
            }
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

/// A candidate's manifest over its sorted record file.
#[derive(Debug)]
struct DiskManifest {
    pages: PathBuf,
    manifest: PathBuf,
    summary: ManifestSummary,
    meter: Arc<SpoolMeter>,
    /// Where the last batch ended: its last digest and the index of the
    /// record after it, so a reader that continues from there seeks
    /// nothing.
    cursor: Option<(NodeId, u64)>,
}

impl DiskManifest {
    /// The index of the first record after `after`: a binary search of the
    /// sorted records, one record read a probe.
    fn seek(&self, file: &File, records: u64, after: &NodeId) -> Result<u64, StageFault> {
        let (mut low, mut high) = (0, records);
        let mut probe = [0; RECORD];
        while low < high {
            let middle = low + (high - low) / 2;
            file.read_exact_at(&mut probe, middle * RECORD as u64)
                .map_err(|error| io(&error))?;
            self.meter.touch(1);
            if parse(&probe).0 <= *after {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low)
    }
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
        let file = match File::open(&self.manifest) {
            Ok(file) => file,
            // Settled or discarded under the reader.
            Err(error) if error.kind() == ErrorKind::NotFound => return Err(StageFault::Ended),
            Err(error) => return Err(io(&error)),
        };
        let records = file.metadata().map_err(|error| io(&error))?.len() / RECORD as u64;
        let start = match (after, self.cursor) {
            (None, _) => 0,
            (Some(after), Some((last, next))) if after == last => next,
            (Some(after), _) => self.seek(&file, records, &after)?,
        };
        let count = (records - start).min(limit as u64);
        let mut batch = vec![0; count as usize * RECORD];
        file.read_exact_at(&mut batch, start * RECORD as u64)
            .map_err(|error| io(&error))?;
        self.meter.touch(count);
        self.meter.holding(batch.len() as u64);
        let mut entries = Vec::with_capacity(count as usize);
        let mut last = None;
        for record in batch.as_chunks::<RECORD>().0 {
            let (id, fresh) = parse(record);
            last = Some(id);
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
        if let Some(last) = last {
            self.cursor = Some((last, start + count));
        }
        Ok(entries)
    }
}
