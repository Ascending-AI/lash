//! Measurement-only actor SQL spike; each run owns and drops a private schema.

use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use clap::Parser;
use futures_util::future::try_join_all;
use serde::Serialize;
use sqlx::{Connection, PgConnection, postgres::PgConnectOptions};
use tokio::time::sleep;

#[path = "postgres_substrate/sql.rs"]
mod sql;
#[path = "postgres_substrate/wake.rs"]
mod wake;

#[derive(Parser)]
#[command(about = "Measure PostgreSQL actor claim, fence, heartbeat/reap and wake patterns")]
struct Args {
    #[arg(long, env = "LASH_POSTGRES_DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[arg(long, default_value = "1,4,16", value_delimiter = ',')]
    nodes: Vec<usize>,
    #[arg(long, default_value_t = 3)]
    seconds: u64,
    #[arg(long, default_value_t = 128)]
    wake_events: usize,
}

#[derive(Serialize)]
struct Distribution {
    count: usize,
    p50_us: u64,
    p99_us: u64,
    max_us: u64,
    samples_us: Vec<u64>,
}

impl Distribution {
    fn new(mut samples: Vec<u64>) -> Result<Self> {
        ensure!(!samples.is_empty(), "measurement has no samples");
        samples.sort_unstable();
        let count = samples.len();
        Ok(Self {
            count,
            p50_us: samples[(count * 50).div_ceil(100) - 1],
            p99_us: samples[(count * 99).div_ceil(100) - 1],
            max_us: samples[count - 1],
            samples_us: samples,
        })
    }
}

#[derive(Serialize)]
struct Receipt {
    operation: String,
    nodes: usize,
    elapsed_s: f64,
    operations_per_s: f64,
    rows_per_s: f64,
    rows: u64,
    empty_claims: u64,
    per_node_operations: Vec<usize>,
    per_node_rows: Vec<u64>,
    latency: Distribution,
    fence_lock: Option<Distribution>,
    lock_observations: u64,
    active_observations: u64,
    monitor_ticks: u64,
    claim_plan: Option<Vec<String>>,
}

fn emit(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[derive(Clone, Copy)]
enum Operation {
    Claim(i64),
    Fence(bool),
    HeartbeatReap,
}

#[derive(Default)]
struct Samples {
    latency: Vec<u64>,
    lock: Vec<u64>,
    rows: u64,
    empty: u64,
}

async fn connect(options: &PgConnectOptions, schema: &str) -> Result<PgConnection> {
    let mut conn = PgConnection::connect_with(options).await?;
    // schema is generated here, never interpolated from a CLI argument.
    sqlx::raw_sql(&format!(
        "SET search_path TO {schema}, public; SET lock_timeout TO '5s'"
    ))
    .execute(&mut conn)
    .await?;
    Ok(conn)
}

async fn step(
    conn: &mut PgConnection,
    node: usize,
    op: Operation,
    samples: &mut Samples,
) -> Result<()> {
    let name = format!("node-{node}");
    let start = Instant::now();
    let rows = match op {
        Operation::Claim(batch) => {
            let claimed: Vec<(String, i64)> = sqlx::query_as(sql::CLAIM)
                .bind(vec!["spike"])
                .bind(batch)
                .bind(name)
                .fetch_all(&mut *conn)
                .await?;
            samples.latency.push(micros(start.elapsed()));
            let keys: Vec<_> = claimed.into_iter().map(|(key, _)| key).collect();
            if keys.is_empty() {
                samples.empty += 1;
            } else {
                // Untimed recycling keeps a steady ready set. Wall throughput includes it.
                sqlx::query(sql::RELEASE)
                    .bind(&keys)
                    .execute(&mut *conn)
                    .await?;
            }
            u64::try_from(keys.len())?
        }
        Operation::Fence(hot) => {
            let key = format!("s/{:06}", if hot { 0 } else { node });
            let mut tx = conn.begin().await?;
            let lock_start = Instant::now();
            let (epoch,): (i64,) = sqlx::query_as(sql::FENCE)
                .bind(&key)
                .fetch_one(&mut *tx)
                .await?;
            samples.lock.push(micros(lock_start.elapsed()));
            // Setup grants epoch 1. A stale grant is refused before any domain write.
            ensure!(epoch == 1, "ownership lost");
            let written = sqlx::query(sql::WRITE)
                .bind(key)
                .bind(vec![0xab_u8; 256])
                .bind(epoch)
                .execute(&mut *tx)
                .await?;
            ensure!(
                written.rows_affected() == 1,
                "fenced write did not advance the actor"
            );
            tx.commit().await?;
            samples.latency.push(micros(start.elapsed()));
            1
        }
        Operation::HeartbeatReap => {
            let changed = sqlx::query(sql::HEARTBEAT)
                .bind(name)
                .execute(&mut *conn)
                .await?;
            ensure!(changed.rows_affected() == 1, "heartbeat lost its node");
            let reaped = sqlx::query(sql::REAP).fetch_all(&mut *conn).await?;
            samples.latency.push(micros(start.elapsed()));
            u64::try_from(reaped.len())? + 1
        }
    };
    samples.rows += rows;
    Ok(())
}

struct Scenario {
    name: String,
    op: Operation,
    actors: i32,
    held: i64,
}

impl Scenario {
    fn new(name: impl Into<String>, op: Operation, actors: i32, held: i64) -> Self {
        Self {
            name: name.into(),
            op,
            actors,
            held,
        }
    }
}

async fn measure(
    options: &PgConnectOptions,
    schema: &str,
    nodes: usize,
    seconds: u64,
    scenario: Scenario,
) -> Result<()> {
    let Scenario {
        name,
        op,
        actors,
        held,
    } = scenario;
    let mut setup = connect(options, schema).await?;
    sqlx::raw_sql("TRUNCATE bench_actors, bench_nodes, phase_state")
        .execute(&mut setup)
        .await?;
    sqlx::query(sql::SEED)
        .bind(actors)
        .execute(&mut setup)
        .await?;
    sqlx::raw_sql("INSERT INTO phase_state SELECT actor_key, 0, decode(repeat('ab',256),'hex') FROM bench_actors")
        .execute(&mut setup).await?;
    for node in 0..nodes {
        sqlx::query(
            "INSERT INTO bench_nodes VALUES ($1,1,'spike',FALSE,lash_now(),lash_now()+3600000)",
        )
        .bind(format!("node-{node}"))
        .execute(&mut setup)
        .await?;
    }
    if matches!(op, Operation::Fence(_)) {
        sqlx::raw_sql("UPDATE bench_actors SET state='owned', epoch=1, ready_at_ms=NULL, owner_node='node-0', owner_incarnation=1")
            .execute(&mut setup).await?;
    }
    sqlx::raw_sql("ANALYZE bench_actors; ANALYZE bench_nodes")
        .execute(&mut setup)
        .await?;
    let claim_plan = if let Operation::Claim(batch) = op {
        let lines: Vec<(String,)> =
            sqlx::query_as(&format!("EXPLAIN (COSTS false) {}", sql::CLAIM))
                .bind(vec!["spike"])
                .bind(batch)
                .bind("plan-node")
                .fetch_all(&mut setup)
                .await?;
        Some(lines.into_iter().map(|(line,)| line).collect())
    } else {
        None
    };
    let mut holder = connect(options, schema).await?;
    // Only the deliberate SKIP LOCKED diagnostic holds a long transaction.
    // A zero-row SELECT in an idle transaction would still pin an MVCC snapshot.
    let mut held_tx = if held > 0 {
        let mut tx = holder.begin().await?;
        sqlx::query("SELECT actor_key FROM bench_actors ORDER BY actor_key LIMIT $1 FOR UPDATE")
            .bind(held)
            .fetch_all(&mut *tx)
            .await?;
        Some(tx)
    } else {
        None
    };
    let mut connections = try_join_all((0..nodes).map(|_| connect(options, schema))).await?;
    try_join_all(
        connections
            .iter_mut()
            .enumerate()
            .map(|(node, conn)| async move {
                let mut discarded = Samples::default();
                for _ in 0..16 {
                    step(conn, node, op, &mut discarded).await?;
                }
                Ok::<_, anyhow::Error>(())
            }),
    )
    .await?;
    let mut observer = connect(options, schema).await?;
    let start = Instant::now();
    let deadline = start + Duration::from_secs(seconds);
    let workers = try_join_all(
        connections
            .iter_mut()
            .enumerate()
            .map(|(node, conn)| async move {
                let mut samples = Samples::default();
                while Instant::now() < deadline {
                    step(conn, node, op, &mut samples).await?;
                }
                Ok::<_, anyhow::Error>(samples)
            }),
    );
    let monitor = async {
        let mut locks = 0_u64;
        let mut active = 0_u64;
        let mut ticks = 0_u64;
        while Instant::now() < deadline {
            let (lock_count, active_count): (i64, i64) = sqlx::query_as(sql::LOCK_SAMPLES)
                .bind(options.get_application_name())
                .fetch_one(&mut observer)
                .await?;
            locks += u64::try_from(lock_count)?;
            active += u64::try_from(active_count)?;
            ticks += 1;
            sleep(Duration::from_millis(5)).await;
        }
        Ok::<_, anyhow::Error>((locks, active, ticks))
    };
    let (results, (locks, active, ticks)) = tokio::try_join!(workers, monitor)?;
    let elapsed = start.elapsed().as_secs_f64();
    if let Some(tx) = held_tx.take() {
        tx.rollback().await?;
    }
    let per_node_operations = results.iter().map(|s| s.latency.len()).collect();
    let per_node_rows = results.iter().map(|s| s.rows).collect();
    let empty = results.iter().map(|s| s.empty).sum();
    let rows = results.iter().map(|s| s.rows).sum();
    let latencies = results
        .iter()
        .flat_map(|s| s.latency.iter().copied())
        .collect();
    let lock_latencies: Vec<_> = results.into_iter().flat_map(|s| s.lock).collect();
    let latency = Distribution::new(latencies)?;
    emit(&Receipt {
        operation: name,
        nodes,
        elapsed_s: elapsed,
        operations_per_s: latency.count as f64 / elapsed,
        rows_per_s: rows as f64 / elapsed,
        rows,
        empty_claims: empty,
        per_node_operations,
        per_node_rows,
        latency,
        fence_lock: if lock_latencies.is_empty() {
            None
        } else {
            Some(Distribution::new(lock_latencies)?)
        },
        lock_observations: locks,
        active_observations: active,
        monitor_ticks: ticks,
        claim_plan,
    })
}

async fn reap_populated(options: &PgConnectOptions, schema: &str, nodes: usize) -> Result<()> {
    let mut conn = connect(options, schema).await?;
    let mut times = Vec::new();
    let start = Instant::now();
    for _ in 0..128 {
        sqlx::raw_sql("TRUNCATE bench_nodes, bench_actors")
            .execute(&mut conn)
            .await?;
        sqlx::query("INSERT INTO bench_nodes SELECT 'dead-'||i,1,'spike',FALSE,0,0 FROM generate_series(1,$1) i")
            .bind(i32::try_from(nodes)?).execute(&mut conn).await?;
        sqlx::query("INSERT INTO bench_actors (actor_key,kind,state,epoch,owner_node,owner_incarnation,formats) SELECT 'p/'||n||'/'||i,'process','owned',1,'dead-'||n,1,'spike' FROM generate_series(1,$1) n CROSS JOIN generate_series(1,16) i")
            .bind(i32::try_from(nodes)?).execute(&mut conn).await?;
        let before = Instant::now();
        let reaped = sqlx::query(sql::REAP).fetch_all(&mut conn).await?;
        times.push(micros(before.elapsed()));
        ensure!(
            reaped.len() == nodes * 16,
            "reap failed to release all dead owners"
        );
        let (invalid,): (i64,) = sqlx::query_as("SELECT count(*) FROM bench_actors WHERE epoch<>2 OR state<>'ready' OR owner_node IS NOT NULL OR owner_incarnation IS NOT NULL")
            .fetch_one(&mut conn).await?;
        ensure!(invalid == 0, "reap failed its epoch fence");
    }
    let elapsed = start.elapsed().as_secs_f64();
    emit(
        &serde_json::json!({"operation":"reap_populated", "nodes":nodes,
        "actors_per_sweep":nodes*16, "setup_inclusive_sweeps_per_s":128.0/elapsed,
        "latency":Distribution::new(times)?}),
    )
}

async fn run(options: &PgConnectOptions, schema: &str, args: &Args) -> Result<()> {
    let mut conn = connect(options, schema).await?;
    sqlx::raw_sql(sql::DDL).execute(&mut conn).await?;
    let settings: Vec<(String, String)> = sqlx::query_as("SELECT name, setting FROM pg_settings WHERE name IN ('server_version','fsync','synchronous_commit','full_page_writes','max_connections','shared_buffers','wal_sync_method') ORDER BY name")
        .fetch_all(&mut conn).await?;
    emit(
        &serde_json::json!({"setup":{"settings":settings, "nodes":args.nodes,
        "seconds_per_phase":args.seconds,"wake_events":args.wake_events,
        "worker_connections_per_node":1,"warmup_operations_per_node":16,
        "latency_unit":"microseconds", "payload_bytes":256,
        "transport":"TCP", "client_parallelism":std::thread::available_parallelism()?.get()}}),
    )?;
    for &nodes in &args.nodes {
        for batch in [1, 16, 64] {
            measure(
                options,
                schema,
                nodes,
                args.seconds,
                Scenario::new(
                    format!("claim_4096_batch_{batch}"),
                    Operation::Claim(batch),
                    4096,
                    0,
                ),
            )
            .await?;
        }
        for scenario in [
            Scenario::new("claim_hot16_batch16", Operation::Claim(16), 16, 0),
            Scenario::new("claim_hot16_half_locked", Operation::Claim(16), 16, 8),
            Scenario::new(
                "fence_distinct",
                Operation::Fence(false),
                i32::try_from(nodes)?,
                0,
            ),
            Scenario::new("fence_hot1", Operation::Fence(true), 1, 0),
            Scenario::new("heartbeat_reap_empty", Operation::HeartbeatReap, 0, 0),
        ] {
            measure(options, schema, nodes, args.seconds, scenario).await?;
        }
        reap_populated(options, schema, nodes).await?;
        wake::measure(options, schema, nodes, args.wake_events).await?;
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.seconds > 0 && args.wake_events >= 100,
        "use positive seconds and at least 100 wake events"
    );
    ensure!(
        !args.nodes.is_empty() && args.nodes.iter().all(|n| (1..=32).contains(n)),
        "nodes must be in 1..=32"
    );
    let schema = format!("s2_{}", uuid::Uuid::new_v4().simple());
    let options: PgConnectOptions = args
        .database_url
        .parse::<PgConnectOptions>()?
        .application_name(&schema);
    let mut admin = PgConnection::connect_with(&options).await?;
    sqlx::raw_sql(&format!("CREATE SCHEMA {schema}"))
        .execute(&mut admin)
        .await?;
    let result = run(&options, &schema, &args).await;
    sqlx::raw_sql(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&mut admin)
        .await?;
    result
}
