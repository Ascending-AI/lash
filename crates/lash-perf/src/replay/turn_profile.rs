//! One-command reader for replay turn phases and physical SQL work.
use super::{ReplayArgs, ReplayHeapArgs, StoreKind};
use anyhow::{Context as _, Result, ensure};
use std::path::PathBuf;

#[derive(Debug, clap::Args)]
pub struct Args {
    pub workload: String,
    #[arg(long, default_value_t = 3)]
    pub turn: usize,
    #[arg(long, value_enum, default_value = "sqlite")]
    pub store: StoreKind,
    #[arg(long)]
    pub out: PathBuf,
    #[arg(long)]
    pub store_dir: PathBuf,
    #[arg(long, env = "LASH_POSTGRES_DATABASE_URL", hide_env_values = true)]
    pub postgres_url: Option<String>,
}

pub async fn run(args: &Args) -> Result<()> {
    ensure!(args.turn > 0, "--turn is one-based");
    let workload = super::script::resolve(&args.workload)?;
    ensure!(
        workload.sessions == 1,
        "profile-turn selects a single-session workload; replay reports concurrent session tails"
    );
    ensure!(
        args.turn <= workload.turn_count(),
        "selected turn is outside this workload"
    );
    ensure!(
        args.store != StoreKind::Postgres || std::env::var_os("KILN_GATE_ID").is_some(),
        "PostgreSQL profiling requires kiln gate"
    );
    let profiles_path = args.out.with_extension("turns.json");
    super::run(&ReplayArgs {
        instructions: false,
        worker_resources: false,
        workloads: vec![args.workload.clone()],
        turns: Some(args.turn),
        store: args.store,
        store_dir: args.store_dir.clone(),
        postgres_url: args.postgres_url.clone(),
        out: args.out.clone(),
        history: None,
        sql_shapes: None,
        traffic: false,
        effect_phases: None,
        effect_phases_off: false,
        heap: ReplayHeapArgs::default(),
        turn_report: Some(profiles_path.clone()),
    })
    .await?;
    let profiles: Vec<lash::tracing::TurnProfile> =
        serde_json::from_slice(&std::fs::read(&profiles_path)?)?;
    let profile = profiles
        .get(args.turn - 1)
        .context("selected turn emitted no physical profile")?;
    ensure!(
        profile.omitted_intervals == 0 && profile.omitted_sql_keys == 0,
        "turn profile exceeded bounded capture; consult the sidecar"
    );
    ensure!(
        profile.sql.iter().map(|r| r.statements).sum::<u64>() == profile.statements,
        "SQL rows do not conserve the turn total"
    );
    print!("{}", table(profile));
    println!("profile-turn: {}", profiles_path.display());
    Ok(())
}

pub fn table(profile: &lash::tracing::TurnProfile) -> String {
    let mut out = String::from(
        "phase                             exclusive_ms statements rows_read rows_affected bind_bytes result_bytes transactions PG_round_trips\n",
    );
    let mut shares = profile.exclusive_ns();
    for row in &profile.sql {
        let key = row.revision.map_or_else(
            || row.phase.clone(),
            |rev| format!("{} revision {rev}", row.phase),
        );
        shares.entry(key).or_default();
    }
    for (phase, ns) in shares {
        let rows: Vec<_> = profile
            .sql
            .iter()
            .filter(|r| {
                let name = r.revision.map_or_else(
                    || r.phase.clone(),
                    |rev| format!("{} revision {rev}", r.phase),
                );
                name == phase
            })
            .collect();
        let sum =
            |f: fn(&lash::tracing::TurnSqlWork) -> u64| rows.iter().map(|r| f(r)).sum::<u64>();
        out.push_str(&format!(
            "{phase:<33} {:>12.3} {:>10} {:>9} {:>13} {:>10} {:>12} {:>12} {:>14}\n",
            ns as f64 / 1e6,
            sum(|r| r.statements),
            sum(|r| r.rows_returned),
            sum(|r| r.rows_affected),
            sum(|r| r.bound_bytes),
            sum(|r| r.result_bytes),
            sum(|r| if r.shape.starts_with("begin") {
                r.statements
            } else {
                0
            }),
            sum(|r| if r.backend == "postgres" {
                r.statements
            } else {
                0
            })
        ));
    }
    out.push_str(&format!("TOTAL                             {:>12.3} {:>10} {:>9} {:>13} {:>10} {:>12} {:>12} {:>14}\n", profile.elapsed_ns as f64 / 1e6,
        profile.statements, profile.sql.iter().map(|r| r.rows_returned).sum::<u64>(), profile.sql.iter().map(|r| r.rows_affected).sum::<u64>(),
        profile.sql.iter().map(|r| r.bound_bytes).sum::<u64>(), profile.sql.iter().map(|r| r.result_bytes).sum::<u64>(), profile.transactions,
        profile.sql.iter().filter(|r| r.backend == "postgres").map(|r| r.statements).sum::<u64>()));
    let mut cuts = vec![0, profile.elapsed_ns];
    for phase in &profile.phases {
        cuts.extend([
            phase.start_ns.min(profile.elapsed_ns),
            phase.end_ns.min(profile.elapsed_ns),
        ]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut path: Vec<(String, u64)> = Vec::new();
    for cut in cuts.windows(2) {
        let selected = profile
            .phases
            .iter()
            .filter(|p| p.start_ns <= cut[0] && p.end_ns >= cut[1])
            .max_by_key(|p| p.depth);
        let name = selected.map_or_else(
            || "owner orchestration".to_owned(),
            |p| {
                p.revision
                    .map_or_else(|| p.phase.clone(), |r| format!("{} revision {r}", p.phase))
            },
        );
        if let Some((last, ns)) = path.last_mut().filter(|(last, _)| *last == name) {
            let _ = last;
            *ns += cut[1] - cut[0];
        } else {
            path.push((name, cut[1] - cut[0]));
        }
    }
    out.push_str(
        "critical wall path (deepest active interval, chronological; not a CPU/dependency DAG):\n",
    );
    for (phase, ns) in path {
        out.push_str(&format!("  {phase}: {:.3} ms\n", ns as f64 / 1e6));
    }
    out.push_str(&format!(
        "installed_graph_bytes={} snapshots={} manifest_calls={} link_payload_calls={}\n",
        profile.store_bytes_written, profile.snapshots, profile.manifests, profile.link_payloads
    ));
    out.push_str("semantic graph writes by phase and owner revision (inserted nodes; committed only on successful attempt)\n");
    for row in &profile.writes {
        out.push_str(&format!(
            "{} | {:?} | {} | nodes={} bytes={}\n",
            row.phase, row.revision, row.kind, row.nodes, row.bytes
        ));
    }
    out.push_str("phase | backend | owner revision | origin | shape | statements | rows_read | rows_affected | bind_bytes | result_bytes\n");
    for row in &profile.sql {
        out.push_str(&format!(
            "{} | {} | {:?} | {} | {} | {} | {} | {} | {} | {}\n",
            row.phase,
            row.backend,
            row.revision,
            row.origin,
            row.shape,
            row.statements,
            row.rows_returned,
            row.rows_affected,
            row.bound_bytes,
            row.result_bytes
        ));
    }
    out.push_str("Wall shares use the deepest active phase; phases sum exactly to the full profile wall; a cold claim can precede send. SQL includes attempted statements. Graph bytes exclude WAL, fsync and SQL payload. PG statement round trips exclude pool checks and protocol control. Store and worker waits are measured where reached; fsync is unavailable. Hot resident owners have no lease claim in this window; lease wait is unknown. Owner attempts are joined to facade admission and readback.\n");
    out
}

/// Place physical owner intervals on the facade's full send/outcome clock.
pub(super) fn merge(
    client: &mut lash::tracing::TurnProfile,
    mut owner: lash::tracing::TurnProfile,
) {
    if owner.started_at_unix_ns < client.started_at_unix_ns {
        let shift = client.started_at_unix_ns - owner.started_at_unix_ns;
        for phase in &mut client.phases {
            phase.start_ns += shift;
            phase.end_ns += shift;
        }
        client.elapsed_ns += shift;
        client.started_at_unix_ns = owner.started_at_unix_ns;
    }
    let offset = owner.started_at_unix_ns - client.started_at_unix_ns;
    for phase in &mut owner.phases {
        phase.start_ns += offset;
        phase.end_ns += offset;
        phase.depth += 10;
    }
    client.phases.extend(owner.phases);
    client.sql.extend(owner.sql);
    client.writes.extend(owner.writes);
    client.statements += owner.statements;
    client.dml += owner.dml;
    client.transactions += owner.transactions;
    client.store_bytes_written += owner.store_bytes_written;
    client.snapshots += owner.snapshots;
    client.manifests += owner.manifests;
    client.link_payloads += owner.link_payloads;
    client.omitted_intervals += owner.omitted_intervals;
    client.omitted_sql_keys += owner.omitted_sql_keys;
}
