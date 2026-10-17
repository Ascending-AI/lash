//! First SQL-work artifact and its maintained text reader, on the replay route.
use anyhow::{Context, Result, ensure};
use lash_core::perf_witness::sql_shapes::work::{Backend, Counts, Receipt};
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead as _, BufReader, BufWriter, Write as _};
use std::path::{Path, PathBuf};

/// Completed windows leave the bounded collector immediately. Only exported
/// keys/counts reach this temporary spool; raw parameters never leave it.
pub(super) struct Spool {
    path: PathBuf,
    writer: BufWriter<File>,
}
impl Spool {
    pub(super) fn new(output: &Path) -> Result<Self> {
        crate::perf_support::report::ensure_parent_dir(output, "SQL work spool")?;
        let path = output.with_extension("work.pending.jsonl");
        let writer = BufWriter::new(File::options().write(true).create_new(true).open(&path)?);
        Ok(Self { path, writer })
    }
    pub(super) fn flush_completed(&mut self) -> Result<()> {
        let chunk = lash_core::perf_witness::sql_shapes::work::drain_completed()
            .context("SQL-shape observer was not installed")?;
        serde_json::to_writer(&mut self.writer, &chunk)?;
        self.writer.write_all(b"\n")?;
        Ok(())
    }
    pub(super) fn finish(mut self, mut work: Receipt) -> Result<Receipt> {
        self.writer.flush()?;
        let mut rows: BTreeMap<_, _> = std::mem::take(&mut work.rows)
            .into_iter()
            .map(|row| (row.key, row.counts))
            .collect();
        for line in BufReader::new(File::open(&self.path)?).lines() {
            let chunk: Receipt = serde_json::from_str(&line?)?;
            ensure!(
                chunk.epoch == work.epoch && chunk.active.is_empty() && chunk.conserves(),
                "SQL-work spool lost its collector or population"
            );
            work.complete &= chunk.complete;
            work.repeat_identities_complete &= chunk.repeat_identities_complete;
            work.admitted_key_bytes = work.admitted_key_bytes.max(chunk.admitted_key_bytes);
            for (backend, counts) in chunk.totals {
                add_counts(work.totals.entry(backend).or_default(), &counts);
            }
            for row in chunk.rows {
                add_counts(rows.entry(row.key).or_default(), &row.counts);
            }
            work.repeated_reads.extend(chunk.repeated_reads);
        }
        work.rows = rows
            .into_iter()
            .map(|(key, counts)| lash_core::perf_witness::sql_shapes::work::Row { key, counts })
            .collect();
        Ok(work)
    }
}
impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
fn add_counts(total: &mut Counts, other: &Counts) {
    total.statements += other.statements;
    total.rows_returned += other.rows_returned;
    total.rows_affected += other.rows_affected;
    total.bound_bytes += other.bound_bytes;
    total.result_bytes += other.result_bytes;
    total.unknown_bindings += other.unknown_bindings;
    total.unqualified_read_identities += other.unqualified_read_identities;
}

pub(super) fn write(
    path: &Path,
    identities: (
        &super::receipt::Identity,
        &crate::runtime_perf::ExperimentIdentity,
        &str,
    ),
    workload: &super::script::Workload,
    backend: &str,
    turns: usize,
    proof: (&Receipt, &serde_json::Value),
) -> Result<()> {
    let (receipt, reference) = proof;
    let (identity, experiment, experiment_id) = identities;
    let mut per_backend = BTreeMap::<Backend, Counts>::new();
    for row in receipt.rows.iter().chain(&receipt.active) {
        let total = per_backend.entry(row.key.backend).or_default();
        add_counts(total, &row.counts);
    }
    let artifact = path.with_file_name("turn-sql-work.json");
    let binary_sha256 = identity
        .executable_sha256
        .as_deref()
        .context("SQL-work export requires the producer's executable identity")?;
    let value = json!({
        "kind": "lash.turn-sql-work", "experiment_id": experiment_id,
        "identity": identity, "experiment": experiment, "binary_sha256": binary_sha256, "pid": std::process::id(),
        "observer": "sql-shapes/execution-join", "backend": backend,
        "worker_scope": "host SQL executor task/connection threads; worker field identifies the starting thread",
        "terminal": "all scripted outcomes successful; session closes attempted; core shutdown completed",
        "workload": workload.name, "script_digest": workload.digest(),
        "turns": turns, "sessions": workload.sessions, "compact_every": workload.compact_every,
        "observation_unit": "one SQL invocation; phase/turn labels are replay round windows, not exclusive async ownership",
        "aggregation": "sum; join statements count once with a relation set; no per-table physical allocation",
        "same_turn_qualification": if workload.sessions == 1 { "single-session replay turn window" } else { "refused: concurrent sessions share a round window" },
        "units": {
            "sqlite": { "bound_bytes": "expanded bound literal UTF-8 bytes, repeated uses included", "result_bytes": "text/blob payload plus eight bytes per numeric scalar", "rows_affected": "sqlite3_total_changes64 delta; includes trigger/FK changes" },
            "postgres": { "bound_bytes": "binary parameter encoder payload, framing excluded", "result_bytes": "returned column-format payload, framing excluded", "rows_affected": "server command completion; SELECT included; hidden trigger/CTE writes unknown" }
        },
        "limits": {"rows": lash_core::perf_witness::sql_shapes::work::WORK_ROW_LIMIT,
            "keys": lash_core::perf_witness::sql_shapes::work::KEY_LIMIT,
            "key_bytes": lash_core::perf_witness::sql_shapes::work::KEY_BYTES_LIMIT,
            "active_executions": lash_core::perf_witness::sql_shapes::work::ACTIVE_LIMIT,
            "pending_bindings": lash_core::perf_witness::sql_shapes::work::ACTIVE_LIMIT},
        "relation_qualification": "only rows with a known relation set qualify; server routine internals are unknown",
        "owner_qualification": "missing owners are unowned work, never assigned to another owner",
        "same_turn_repeated_reads": if workload.sessions == 1 { Some(receipt.repeated_reads.iter().filter(|r| r.key.window.phase == "turn").collect::<Vec<_>>()) } else { None },
        "conserved": receipt.conserves() && reference["conserved"] == true, "reference": reference,
        "read_identity_coverage": coverage(receipt), "per_backend": per_backend, "work": receipt,
    });
    std::fs::write(&artifact, serde_json::to_vec_pretty(&value)?)
        .with_context(|| format!("write {}", artifact.display()))?;
    let text = format!(
        "Experiment {experiment_id} backend={backend} workload={} script={} observer=sql-shapes/execution-join binary_sha256={binary_sha256} terminal=scripted-success witness_conserved={}\n{}",
        workload.name,
        workload.digest(),
        reference["conserved"],
        table(receipt)
    );
    std::fs::write(artifact.with_extension("txt"), &text)?;
    print!("{text}");
    println!("SQL work: {}", artifact.display());
    ensure!(
        reference["conserved"] == true,
        "execution joins differ from independent statement/returned-row/completed-connection witnesses; retained refused artifact"
    );
    ensure!(
        receipt.conserves(),
        "SQL-work counters do not conserve; retained the refused artifact"
    );
    ensure!(
        receipt.complete,
        "SQL-work attribution incomplete (active={}, lost={}, unknown binds={}); retained the refused artifact",
        receipt.active.len(),
        receipt.lost,
        receipt.unknown_bindings()
    );
    Ok(())
}

fn table(receipt: &Receipt) -> String {
    let mut out = format!(
        "SQL work: joins_conserved={} complete={} statements={} active={} lost={} repeat_identities_complete={}\nTop 10 execution joins by statement count (per backend payload units; sum):\n",
        receipt.conserves(),
        receipt.complete,
        receipt.statements(),
        receipt.active.len(),
        receipt.lost,
        receipt.repeat_identities_complete
    );
    let mut rows: Vec<_> = receipt.rows.iter().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.counts.statements));
    for row in rows.iter().take(10) {
        let k = &row.key;
        let c = &row.counts;
        let _ = writeln!(
            out,
            "{} turn={:?} phase={} stmt={} site={} relations={:?} owner={:?} worker={} calls={} returned={} affected={} bind_B={} result_B={} finished={:?} disposition={:?}",
            k.backend,
            k.started_in.turn,
            k.started_in.phase,
            &k.statement[..12],
            k.site,
            k.relations,
            k.owner,
            k.worker,
            c.statements,
            c.rows_returned,
            c.rows_affected,
            c.bound_bytes,
            c.result_bytes,
            k.finished_in,
            k.disposition
        );
    }
    let _ = writeln!(
        out,
        "Read identity coverage over all windows: {}",
        coverage(receipt)
    );
    out.push_str("Exact same-turn repeated reads (phase=turn): exact SQL + params + origin; SQLite TEXT exact modulo embedded NUL; SQLite REAL/unknown encodings excluded; no cache-validity claim\n");
    let _ = writeln!(
        out,
        "Read bind classes (invocations, including excluded): {:?}",
        receipt.read_bind_classes
    );
    let mut repeat_classes = BTreeMap::<String, (u64, u64)>::new();
    for r in &receipt.repeated_reads {
        let count = repeat_classes.entry(r.key.bind_class.clone()).or_default();
        count.0 += 1;
        count.1 += r.executions;
    }
    let _ = writeln!(
        out,
        "Repeated bind classes (groups, executions): {repeat_classes:?}"
    );
    let mut reads: Vec<_> = receipt.repeated_reads.iter().collect();
    reads.sort_by_key(|r| std::cmp::Reverse(r.extra_executions));
    for r in reads
        .iter()
        .filter(|r| r.key.window.phase == "turn")
        .take(10)
    {
        let _ = writeln!(
            out,
            "turn={:?} phase={} stmt={} site={} parameters={} executions={} extra={}",
            r.key.window.turn,
            r.key.window.phase,
            &r.key.statement[..12],
            r.key.site,
            r.key.parameters,
            r.executions,
            r.extra_executions
        );
    }
    out
}

fn coverage(receipt: &Receipt) -> serde_json::Value {
    let reads: u64 = receipt.read_bind_classes.values().sum();
    let repeated: u64 = receipt.repeated_reads.iter().map(|r| r.executions).sum();
    let rows: Vec<_> = receipt.read_bind_classes.iter().map(|(class, n)| {
        let groups: Vec<_> = receipt.repeated_reads.iter().filter(|r| &r.key.bind_class == class).collect();
        let executions: u64 = groups.iter().map(|r| r.executions).sum();
        json!({"bind_class": class, "read_candidates": n,
            "share_of_read_candidates": (reads > 0).then(|| *n as f64 / reads as f64),
            "reported_repeated_groups": groups.len(), "reported_repeated_executions": executions,
            "share_of_reported_repeated_executions": (repeated > 0).then(|| executions as f64 / repeated as f64)})
    }).collect();
    json!({"scope": "all observed windows; SELECT/WITH candidates on PostgreSQL; excluded identities remain in the denominator", "classes": rows})
}
