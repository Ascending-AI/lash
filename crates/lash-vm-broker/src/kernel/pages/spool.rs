//! The activation's private staging spool (S5, `D-SPOOL`): sealed pages
//! held for the parent until a publication settles them.
//!
//! A spool is content addressed. While its quota charge stays within its
//! resident bound ([`RESIDENT_BYTES`], or its quota if smaller) it holds
//! its staged pages and their reference counts in the parent's memory, so
//! a cell that stages, publishes and settles them opens no file per page
//! (FIG-6902). The first seal that would pass the bound moves every page
//! and count into the spool's directory, once, and the spool stays there
//! for the rest of its life: each staged page is then one file named by
//! its digest, and the directory is the index, so the parent's memory
//! holds the spool's usage and nothing per page. A page is written
//! straight to its own file, so a buffer holds at most the page being
//! written; a write that fails removes its file. A quota bounds the
//! spool's contents in either place: each staged page's bytes, plus
//! [`ENTRY_BYTES`] of manifest scratch for each staged page and each
//! distinct external reference (FIG-6512). A publication's install writes
//! its edge log within that scratch, beside the sorted manifest
//! (FIG-6596).
//!
//! A candidate is built at a park: a walk of the candidate root's staged
//! DAG that stops at committed references, appending the record of each
//! page it reaches to a file that is also the walk's queue. A page that
//! more than one staged page names is marked when first reached, in a
//! marker directory bucketed by the digest's first byte; any other page is
//! reached once. An external sort with bounded runs orders the records into
//! the manifest file, ascending by digest, which is the store's blob lock
//! order; the manifest streams from any cursor, and settlement streams it
//! again to delete the candidate's pages. No step holds more than a fixed
//! number of records in memory (FIG-6421). Staged pages no candidate
//! reaches stay until the spool's owner names the roots it still needs
//! ([`StageSpool::retain`]), so the cells of one session can share a spool.
//!
//! A spool lives exactly as long as its stage lease: an authoritative
//! commit deletes the candidate's pages, an authoritative refusal discards
//! the spool with the activation, and an ambiguous publication keeps it. It
//! never survives its parent process, in memory or on disk: a parent that
//! dies takes its resident pages with it, and a parent that opens its
//! [`SpoolRoot`] reaps the directories of dead parents. Their activations
//! resume from their committed roots.
#![expect(
    clippy::disallowed_methods,
    reason = "the spool is the host-supplied staging directory's sole reader and writer (D-SPOOL)"
)]

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lash_core_store::checkpoint_graph::stage::{
    CandidateManifest, EDGE_RECORD_BYTES, EdgeLog, ManifestEntry, ManifestSummary, StageFault,
    StageLeaseId, StageSpool, StageUsage,
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
// An install's edge log record for a fresh page fits, with the page's
// manifest record, in the page's entry charge (FIG-6596).
const _: () = assert!(RECORD as u64 + EDGE_RECORD_BYTES <= ENTRY_BYTES);
/// The records one sorted run holds in memory: the sort's one buffer that
/// grows with its input, up to this bound.
const RUN_RECORDS: usize = 4096;
/// The runs one merge reads at once.
const MERGE_FANIN: usize = 64;
/// The buffer a streamed record file reads or writes through.
const RECORD_BUFFER: usize = 8 << 10;
/// The most quota charge a spool holds in the parent's memory before it
/// moves to its directory (FIG-6902): a 200,000-record cell stages about
/// 42 MB.
pub(crate) const RESIDENT_BYTES: u64 = 64 << 20;

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
    /// The charge each spool holds in memory before it moves to disk.
    resident: u64,
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
    /// page and distinct external reference. A spool holds up to
    /// [`RESIDENT_BYTES`] of that charge in memory. The spools of parents that
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
            resident: RESIDENT_BYTES.min(quota),
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

    /// This root with spools that hold at most `bytes` of charge in
    /// memory, so a law can stage past the bound with a few pages.
    #[cfg(test)]
    pub(crate) fn resident_within(mut self, bytes: u64) -> Self {
        self.resident = bytes;
        self
    }

    /// The spool of the activation `lease` names, empty and resident.
    ///
    /// # Errors
    ///
    /// [`StageFault::Io`], also when the lease already has a spool.
    pub fn spool(&self, lease: StageLeaseId) -> Result<DiskSpool, StageFault> {
        let dir = self.dir.join(format!("{:016x}", lease.0));
        fs::create_dir(&dir).map_err(|error| io(&error))?;
        Ok(DiskSpool {
            lease,
            dir,
            resident: self.resident,
            tier: Tier::Resident(Resident::default()),
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
    pages: Pages,
}

/// The staged pages and reference counts of a spool within its resident
/// bound.
#[derive(Debug, Default)]
struct Resident {
    pages: HashMap<NodeId, Arc<[u8]>>,
    refs: HashMap<NodeId, u64>,
}

/// Where a spool's staged pages and reference counts are: in memory until
/// its charge would pass its resident bound, then in its directory for the
/// rest of its life.
#[derive(Debug)]
enum Tier {
    Resident(Resident),
    Files,
}

/// Where a candidate's manifest reads its fresh pages: the bytes its walk
/// reached in memory, or the spool's page files.
#[derive(Clone, Debug)]
enum Pages {
    Resident(Arc<HashMap<NodeId, Arc<[u8]>>>),
    Files(PathBuf),
}

impl Pages {
    fn read(&self, id: &NodeId) -> Result<Arc<[u8]>, StageFault> {
        match self {
            Self::Resident(pages) => pages.get(id).cloned().ok_or(StageFault::Missing(*id)),
            Self::Files(dir) => match fs::read(dir.join(id.to_hex())) {
                Ok(bytes) => Ok(bytes.into()),
                Err(error) if error.kind() == ErrorKind::NotFound => Err(StageFault::Missing(*id)),
                Err(error) => Err(io(&error)),
            },
        }
    }
}

/// One activation's spool: in the parent's memory within its resident
/// bound, on disk past it.
#[derive(Debug)]
pub struct DiskSpool {
    lease: StageLeaseId,
    dir: PathBuf,
    /// The charge the spool holds in memory before it moves to disk.
    resident: u64,
    tier: Tier,
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

    /// Whether the spool has moved its pages to its directory.
    #[cfg(test)]
    pub(crate) fn on_disk(&self) -> bool {
        matches!(self.tier, Tier::Files)
    }

    fn pages(&self) -> PathBuf {
        self.dir.join("pages")
    }

    fn page(&self, id: &NodeId) -> PathBuf {
        self.pages().join(id.to_hex())
    }

    fn refs(&self, id: &NodeId) -> PathBuf {
        self.dir.join("refs").join(id.to_hex())
    }

    /// Incoming edges from staged pages, including edges to staged children.
    /// On disk the count is the target of a symbolic link named by the
    /// child's digest, so one call reads it and one writes it. A link adds
    /// no content bytes beyond the entry's charge, and the index stays on
    /// disk rather than growing a per-entry map in the parent.
    fn references(&self, id: &NodeId) -> Result<u64, StageFault> {
        let Tier::Resident(resident) = &self.tier else {
            let count = match fs::read_link(self.refs(id)) {
                Ok(count) => count,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
                Err(error) => return Err(io(&error)),
            };
            return count
                .to_str()
                .and_then(|count| count.parse().ok())
                .ok_or_else(|| StageFault::Io("a staging reference count is invalid".to_owned()));
        };
        Ok(resident.refs.get(id).copied().unwrap_or(0))
    }

    /// Moves `id`'s count from `old`, which [`Self::references`] read, one
    /// edge up or down. `staged` is whether `id` is a staged page: an
    /// unstaged child holds its entry exactly while it is referenced.
    fn reference(
        &mut self,
        id: &NodeId,
        old: u64,
        add: bool,
        staged: bool,
    ) -> Result<(), StageFault> {
        let next = if add { old + 1 } else { old - 1 };
        match &mut self.tier {
            Tier::Resident(resident) if next == 0 => {
                resident.refs.remove(id);
            }
            Tier::Resident(resident) => {
                resident.refs.insert(*id, next);
            }
            Tier::Files => {
                let path = self.refs(id);
                match (old, next) {
                    (0, _) => std::os::unix::fs::symlink(next.to_string(), &path)
                        .map_err(|error| io(&error))?,
                    (_, 0) => fs::remove_file(&path).map_err(|error| io(&error))?,
                    _ => {
                        let partial = self.dir.join("refs").join(format!(".next-{}", id.to_hex()));
                        std::os::unix::fs::symlink(next.to_string(), &partial)
                            .map_err(|error| io(&error))?;
                        fs::rename(&partial, &path).map_err(|error| io(&error))?;
                    }
                }
            }
        }
        match (old, next) {
            (0, _) => self.entries += u64::from(!staged),
            (_, 0) => self.entries -= u64::from(!staged),
            _ => {}
        }
        Ok(())
    }

    fn staged(&self, id: &NodeId) -> bool {
        match &self.tier {
            Tier::Resident(resident) => resident.pages.contains_key(id),
            Tier::Files => self.page(id).is_file(),
        }
    }

    /// Stages `bytes` as the page `id`, which is not staged yet.
    fn write(&mut self, id: NodeId, bytes: Arc<[u8]>) -> Result<(), StageFault> {
        let Tier::Resident(resident) = &mut self.tier else {
            return write_page(&self.page(&id), &bytes);
        };
        resident.pages.insert(id, bytes);
        Ok(())
    }

    /// Moves every resident page and count into the spool's directory, the
    /// spool's one move (FIG-6902). A move that fails leaves the spool
    /// resident, with what it wrote removed.
    fn move_to_disk(&mut self) -> Result<(), StageFault> {
        let Tier::Resident(resident) = &self.tier else {
            return Ok(());
        };
        let written = (|| {
            fs::create_dir(self.pages()).map_err(|error| io(&error))?;
            fs::create_dir(self.dir.join("refs")).map_err(|error| io(&error))?;
            for (id, bytes) in &resident.pages {
                write_page(&self.page(id), bytes)?;
            }
            for (id, count) in &resident.refs {
                std::os::unix::fs::symlink(count.to_string(), self.refs(id))
                    .map_err(|error| io(&error))?;
            }
            Ok(())
        })();
        if let Err(fault) = written {
            let _ = fs::remove_dir_all(self.pages());
            let _ = fs::remove_dir_all(self.dir.join("refs"));
            return Err(fault);
        }
        self.tier = Tier::Files;
        Ok(())
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
                pages: candidate.pages.clone(),
                manifest: self.candidate_dir(commit).join("manifest"),
                summary: candidate.summary,
                meter: Arc::clone(&self.meter),
                cursor: None,
            }) as Box<dyn CandidateManifest>
        })
    }

    /// Deletes the staged page `id`, if staged.
    fn remove(&mut self, id: &NodeId) -> Result<(), StageFault> {
        let bytes = match self.read(id) {
            Ok(bytes) => bytes,
            Err(StageFault::Missing(_)) => return Ok(()),
            Err(fault) => return Err(fault),
        };
        let envelope =
            parse_envelope(&bytes, Some(*id)).map_err(|_| StageFault::HashMismatch(*id))?;
        match &mut self.tier {
            Tier::Resident(resident) => {
                resident.pages.remove(id);
            }
            Tier::Files => fs::remove_file(self.page(id)).map_err(|error| io(&error))?,
        }
        self.entries -= u64::from(self.references(id)? == 0);
        for child in envelope.children.iter() {
            let old = self.references(&child)?;
            // Only the last edge's removal asks whether the child is staged.
            let staged = old == 1 && self.staged(&child);
            self.reference(&child, old, false, staged)?;
        }
        self.usage.pages -= 1;
        self.usage.bytes -= bytes.len() as u64;
        Ok(())
    }

    /// The staged pages, by digest.
    fn staged_ids(&self) -> Result<Vec<NodeId>, StageFault> {
        let Tier::Resident(resident) = &self.tier else {
            let mut ids = Vec::new();
            for entry in fs::read_dir(self.pages()).map_err(|error| io(&error))? {
                let name = entry.map_err(|error| io(&error))?.file_name();
                ids.extend(name.to_str().and_then(NodeId::from_hex));
            }
            return Ok(ids);
        };
        Ok(resident.pages.keys().copied().collect())
    }
}

/// Writes a page's file, which must not exist; a write that fails removes
/// it.
fn write_page(target: &Path, bytes: &[u8]) -> Result<(), StageFault> {
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|error| io(&error))?;
    if let Err(error) = file.write_all(bytes) {
        drop(file);
        let _ = fs::remove_file(target);
        return Err(io(&error));
    }
    Ok(())
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
        // Each child is looked up once: whether it is staged, then its
        // count. The canonical child table is distinct, so no child's count
        // moves before its own update below.
        let mut counted = Vec::with_capacity(envelope.children.len());
        for child in envelope.children.iter() {
            let staged = self.staged(&child);
            if !staged && !protected(&child) {
                return Err(StageFault::Unauthorized {
                    page: page.id,
                    child,
                });
            }
            counted.push((child, staged, self.references(&child)?));
        }
        // Count the union of staged pages and their direct children before
        // writing anything; an external reference that becomes staged
        // already occupies its entry.
        let own_entry = u64::from(self.references(&page.id)? == 0);
        let added = own_entry
            + counted
                .iter()
                .filter(|(_, staged, old)| !staged && *old == 0)
                .count() as u64;
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
        if charged > self.resident {
            self.move_to_disk()?;
        }
        self.write(page.id, page.bytes)?;
        self.entries += own_entry;
        for (child, staged, old) in counted {
            self.reference(&child, old, true, staged)?;
        }
        #[cfg(feature = "perf-witness")]
        self.spooled_bytes.fetch_add(len, Ordering::Relaxed);
        self.usage.pages += 1;
        self.usage.bytes += len;
        Ok(())
    }

    fn read(&self, page: &NodeId) -> Result<Arc<[u8]>, StageFault> {
        match &self.tier {
            Tier::Resident(resident) => resident
                .pages
                .get(page)
                .cloned()
                .ok_or(StageFault::Missing(*page)),
            Tier::Files => Pages::Files(self.pages()).read(page),
        }
    }

    fn contains(&self, page: &NodeId) -> bool {
        self.staged(page)
    }

    fn retain(&mut self, roots: &[NodeId]) -> Result<(), StageFault> {
        // Mark what the roots reach, then delete every unmarked page; the
        // stack holds digests, never page bytes.
        let mut reached = Reached::open(self)?;
        let mut stack: Vec<NodeId> = roots.iter().copied().filter(|id| self.staged(id)).collect();
        for id in &stack {
            reached.mark(id)?;
        }
        while let Some(id) = stack.pop() {
            let bytes = self.read(&id)?;
            let envelope =
                parse_envelope(&bytes, Some(id)).map_err(|_| StageFault::HashMismatch(id))?;
            for child in envelope.children.iter() {
                if self.staged(&child) && reached.mark(&child)? {
                    stack.push(child);
                }
            }
        }
        let dead: Vec<NodeId> = self
            .staged_ids()?
            .into_iter()
            .filter(|id| !reached.contains(id))
            .collect();
        for id in dead {
            self.remove(&id)?;
        }
        reached.close()
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
        // One exclusive create both tests and sets a mark; a bucket is made
        // the first time a mark misses it.
        let mark = |id: &NodeId, fresh: bool| -> Result<bool, StageFault> {
            let bucket = marks.join(hex(&id.0[..1]));
            let path = bucket.join(format!("{}.{}", id.to_hex(), if fresh { 'f' } else { 'e' }));
            let create = || File::options().write(true).create_new(true).open(&path);
            match create() {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(false),
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    fs::create_dir(&bucket).map_err(|error| io(&error))?;
                    create().map_err(|error| io(&error))?;
                }
                Err(error) => return Err(io(&error)),
            }
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
        // A resident candidate keeps the fresh pages its walk reached, so
        // its manifest reads them whatever the spool does next.
        let mut reached = matches!(self.tier, Tier::Resident(_)).then(HashMap::new);
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
            if let Some(reached) = &mut reached {
                reached.insert(id, Arc::clone(&bytes));
            }
            summary.fresh_nodes += 1;
            summary.fresh_bytes += bytes.len() as u64;
            summary.fresh_edges += envelope.children.len() as u64;
            for child in envelope.children.iter() {
                let fresh = self.staged(&child);
                // A child's count is the staged pages naming it, so a child
                // only this page names is reached once, here: only a shared
                // child needs a mark.
                if self.references(&child)? == 1 || mark(&child, fresh)? {
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
        let pages = match reached {
            Some(reached) => Pages::Resident(Arc::new(reached)),
            None => Pages::Files(self.pages()),
        };
        self.candidate = Some(Candidate { summary, pages });
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

/// The pages a retention walk reached: a set in memory for a resident
/// spool, marker files in a scratch directory for a spool on disk.
enum Reached {
    Set(HashSet<NodeId>),
    Dir(PathBuf),
}

impl Reached {
    fn open(spool: &DiskSpool) -> Result<Self, StageFault> {
        if matches!(spool.tier, Tier::Resident(_)) {
            return Ok(Self::Set(HashSet::new()));
        }
        let dir = spool.dir.join("retain");
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(io(&error)),
        }
        fs::create_dir(&dir).map_err(|error| io(&error))?;
        Ok(Self::Dir(dir))
    }

    /// Marks `id`, whether it was unmarked.
    fn mark(&mut self, id: &NodeId) -> Result<bool, StageFault> {
        match self {
            Self::Set(set) => Ok(set.insert(*id)),
            Self::Dir(dir) => {
                let marker = dir.join(id.to_hex());
                if marker.is_file() {
                    return Ok(false);
                }
                fs::write(&marker, []).map_err(|error| io(&error))?;
                Ok(true)
            }
        }
    }

    fn contains(&self, id: &NodeId) -> bool {
        match self {
            Self::Set(set) => set.contains(id),
            Self::Dir(dir) => dir.join(id.to_hex()).is_file(),
        }
    }

    fn close(self) -> Result<(), StageFault> {
        match self {
            Self::Set(_) => Ok(()),
            Self::Dir(dir) => fs::remove_dir_all(&dir).map_err(|error| io(&error)),
        }
    }
}

/// A candidate's manifest over its sorted record file.
#[derive(Debug)]
struct DiskManifest {
    pages: Pages,
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
                ManifestEntry::Fresh(EncodedNode {
                    id,
                    bytes: self.pages.read(&id)?,
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

    fn fresh_range(&mut self, id: &NodeId, offset: u64, len: usize) -> Result<Vec<u8>, StageFault> {
        let dir = match &self.pages {
            Pages::Resident(pages) => {
                let bytes = pages.get(id).ok_or(StageFault::Missing(*id))?;
                return usize::try_from(offset)
                    .ok()
                    .and_then(|start| bytes.get(start..start.checked_add(len)?))
                    .map(<[u8]>::to_vec)
                    .ok_or_else(|| {
                        StageFault::Io("a fresh page range is out of bounds".to_owned())
                    });
            }
            Pages::Files(dir) => dir,
        };
        let file = match File::open(dir.join(id.to_hex())) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(StageFault::Missing(*id));
            }
            Err(error) => return Err(io(&error)),
        };
        let mut range = vec![0; len];
        file.read_exact_at(&mut range, offset)
            .map_err(|error| io(&error))?;
        Ok(range)
    }

    fn edge_log(&mut self) -> Result<Box<dyn EdgeLog>, StageFault> {
        let file = match File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(self.manifest.with_file_name("edges"))
        {
            Ok(file) => file,
            // Settled or discarded under the reader.
            Err(error) if error.kind() == ErrorKind::NotFound => return Err(StageFault::Ended),
            Err(error) => return Err(io(&error)),
        };
        Ok(Box::new(DiskEdgeLog { file, len: 0 }))
    }
}

/// An install's edge log beside its candidate's manifest: at most
/// [`EDGE_RECORD_BYTES`] for each fresh page with children, which the
/// two records of scratch each entry's charge keeps free once the manifest
/// is sorted cover. A later install truncates it; settlement and withdrawal
/// delete it with the candidate.
#[derive(Debug)]
struct DiskEdgeLog {
    file: File,
    len: u64,
}

impl EdgeLog for DiskEdgeLog {
    fn append(&mut self, bytes: &[u8]) -> Result<(), StageFault> {
        self.file
            .write_all_at(bytes, self.len)
            .map_err(|error| io(&error))?;
        self.len += bytes.len() as u64;
        Ok(())
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), StageFault> {
        self.file
            .read_exact_at(out, offset)
            .map_err(|error| io(&error))
    }
}
