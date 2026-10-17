//! Sidecar lifetime: successful and interrupted populations keep the same identity.
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

pub(super) struct Capture {
    path: PathBuf,
    header: Value,
    pool: Option<lash::vm::WorkerPool>,
    turns: Vec<Value>,
    finished: bool,
}

fn artifact(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

impl Capture {
    pub fn new(
        path: &Path,
        off: bool,
        workload: &super::script::Workload,
        identity: &super::receipt::Identity,
    ) -> Result<Self> {
        Ok(Self {
            path: path.to_owned(),
            header: json!({
                "kind": "lash.turn-effect-phases", "identity": identity,
                "host_artifact_sha256": artifact(&std::env::current_exe()?)?,
                "observer": if off { "off; identity receipts only" } else { "bounded host-clock effect phases" },
                "workload": workload.name, "script_digest": workload.digest(),
                "host_pid": std::process::id(),
            }),
            pool: None,
            turns: Vec::new(),
            finished: false,
        })
    }
    pub fn attach(
        &mut self,
        pool: Option<&lash::vm::WorkerPool>,
        backend: &str,
        experiment: &crate::runtime_perf::ExperimentIdentity,
    ) -> Result<()> {
        let pool = pool.context("effect phases require a code-mode worker")?;
        pool.enable_execution_receipts(32_768);
        self.header["backend"] = json!(backend);
        self.header["experiment"] = json!(experiment);
        self.header["worker_artifact_sha256"] = json!(artifact(&pool.config().entry.executable)?);
        self.header["pool_epoch"] = json!(pool.measurements().epoch);
        self.header["before"] = json!(pool.measurements());
        self.pool = Some(pool.clone());
        Ok(())
    }
    pub fn cut(&self) -> Option<usize> {
        self.pool
            .as_ref()?
            .effect_phases()
            .map(|report| report.events.len())
    }
    pub fn turn(
        &mut self,
        turn: usize,
        first: Option<usize>,
        sessions: usize,
        result_digests: Vec<String>,
    ) {
        self.turns.push(json!({
            "turn": turn, "first_event": first, "through_event": self.cut(), "result_digests": result_digests,
            "terminal": "all session outcomes successful; scripted calls matched", "sessions": sessions,
        }));
    }
    pub fn finish(mut self) -> Result<()> {
        self.write("all selected turns successful; scripted calls matched")?;
        self.finished = true;
        Ok(())
    }
    fn write(&self, terminal: &str) -> Result<()> {
        let pool = self.pool.as_ref().context("worker pool never started")?;
        let report = pool.effect_phases();
        let mut header = self.header.clone();
        header["terminal"] = json!(terminal);
        header["turns"] = json!(self.turns);
        header["after"] = json!(pool.measurements());
        header["clock"] = json!(report.as_ref().map(|r| r.clock));
        header["capacity"] = json!(report.as_ref().map(|r| r.capacity));
        header["dropped"] = json!(report.as_ref().map(|r| r.dropped));
        crate::perf_support::report::ensure_parent_dir(&self.path, "effect phases")?;
        let mut out = std::io::BufWriter::new(std::fs::File::create(&self.path)?);
        writeln!(out, "{header}")?;
        for event in report.into_iter().flat_map(|report| report.events) {
            writeln!(out, "{}", serde_json::to_string(&event)?)?;
        }
        out.flush()?;
        Ok(())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if !self.finished {
            // Preserve the failure, including loss events, without replacing the
            // original runtime error with a diagnostic export error.
            let _ = self.write("incomplete; replay did not finish");
        }
    }
}
