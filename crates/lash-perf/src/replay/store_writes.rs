//! Profile-only attribution of a replay's physical store writes, per window
//! (a turn, the retirement after it, a compaction and the retirement after
//! it), to the tables and indexes that own them. Not gated, and never on an
//! ordinary replay's path.
//!
//! SQLite: the write observer's WAL body bytes per page (so `--traffic`),
//! each page owned by its `dbstat` entry at the window's end cut, else at
//! its start; a page in neither was freed. PostgreSQL: WAL record bytes from
//! `pg_walinspect`, owned by the relation of each record's first block; WAL
//! page headers and alignment make up the rest of the window's LSN advance.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use lash_protocol_types::sync::MutexExt as _;
use lash_sqlite_store::write_observer::WriteKind;
use serde::Serialize;

use super::meter::{Reading, StoreProbe};

pub(super) struct StoreWrites {
    source: Source,
    /// Page owners of every written SQLite file at the last window's end.
    owners: BTreeMap<String, BTreeMap<u32, String>>,
    windows: Vec<Window>,
}

enum Source {
    Sqlite { main: PathBuf },
    Postgres { pool: sqlx::PgPool },
}

/// One meter cut: its counters and its write-observer reading.
pub(super) type Cut<'a> = (&'a Reading, Option<&'a super::traffic::Reading>);

#[derive(Serialize)]
struct Window {
    turn: usize,
    window: &'static str,
    /// The window's `store_bytes_written` share.
    bytes: u64,
    owners: BTreeMap<String, u64>,
}

impl StoreWrites {
    pub(super) async fn new(probe: &StoreProbe, traffic: bool) -> Result<Self> {
        let source = match probe {
            StoreProbe::Sqlite { connection, .. } => {
                ensure!(
                    traffic,
                    "SQLite store-write attribution reads the write observer: add --traffic"
                );
                Source::Sqlite {
                    main: connection
                        .lock_recover()
                        .path()
                        .context("the SQLite probe has no database file")?
                        .into(),
                }
            }
            StoreProbe::Postgres { pool } => {
                // Before the first cut: the extension's own catalog rows
                // belong to no window.
                sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_walinspect")
                    .execute(pool)
                    .await
                    .context("store-write attribution needs pg_walinspect")?;
                Source::Postgres { pool: pool.clone() }
            }
        };
        Ok(Self {
            source,
            owners: BTreeMap::new(),
            windows: Vec::new(),
        })
    }

    /// Attribute the writes between two cuts. Call it right after the `to`
    /// cut, before the store writes again: SQLite page owners are read now
    /// and stand for the `from` cut of the next window.
    pub(super) async fn window(
        &mut self,
        turn: usize,
        window: &'static str,
        (before, before_traffic): Cut<'_>,
        (after, after_traffic): Cut<'_>,
    ) -> Result<()> {
        let mut owners = BTreeMap::<String, u64>::new();
        let bytes = match &self.source {
            Source::Sqlite { main } => {
                let (Some(start_receipt), Some(end_receipt)) = (
                    before_traffic.and_then(|traffic| traffic.sqlite.as_ref()),
                    after_traffic.and_then(|traffic| traffic.sqlite.as_ref()),
                ) else {
                    anyhow::bail!("no SQLite write observer reading at a cut");
                };
                let interval = end_receipt
                    .difference(start_receipt)
                    .context("incomplete SQLite write attribution")?;
                let mut end = BTreeMap::new();
                for row in &end_receipt.rows {
                    if row.key.kind == WriteKind::WalBody && !end.contains_key(&row.key.database) {
                        end.insert(row.key.database.clone(), page_owners(&row.key.database)?);
                    }
                }
                for row in &interval.rows {
                    if row.key.kind != WriteKind::WalBody {
                        continue;
                    }
                    let page = row.key.page.context("a WAL body write has no page")?;
                    let owner = [&end, &self.owners]
                        .iter()
                        .find_map(|owners| owners.get(&row.key.database)?.get(&page))
                        .map_or("<freelist>", String::as_str);
                    let database = Path::new(&row.key.database);
                    let owner = if database == main {
                        owner.to_owned()
                    } else {
                        format!("{}:{owner}", database.display())
                    };
                    *owners.entry(owner).or_default() += row.bytes;
                }
                self.owners = end;
                interval.bytes(WriteKind::WalBody)
            }
            Source::Postgres { pool } => {
                let position = |reading: &Reading| {
                    reading
                        .get("store_bytes_written")
                        .map(|position| *position as u64)
                        .context("the PostgreSQL reading has no WAL position")
                };
                let (start, end) = (position(before)?, position(after)?);
                let rows: Vec<(String, i64)> = sqlx::query_as(PG_OWNERS)
                    .bind(pg_lsn(start))
                    .bind(pg_lsn(end))
                    .fetch_all(pool)
                    .await
                    .context("read the window's WAL records")?;
                for (owner, bytes) in rows {
                    owners.insert(owner, bytes as u64);
                }
                let bytes = end - start;
                let records: u64 = owners.values().sum();
                ensure!(
                    records <= bytes,
                    "WAL records exceed the window's LSN advance"
                );
                if records < bytes {
                    owners.insert(
                        "<WAL page headers and alignment>".to_owned(),
                        bytes - records,
                    );
                }
                bytes
            }
        };
        self.windows.push(Window {
            turn,
            window,
            bytes,
            owners,
        });
        Ok(())
    }

    /// Write every window to `path`.
    pub(super) fn finish(self, path: &Path) -> Result<()> {
        crate::perf_support::report::ensure_parent_dir(path, "store writes")?;
        std::fs::write(path, serde_json::to_vec_pretty(&self.windows)?)
            .with_context(|| format!("write {}", path.display()))
    }
}

/// The windows of `turn` in `path`, largest owners first.
pub(super) fn table(path: &Path, turn: usize) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Window {
        turn: usize,
        window: String,
        bytes: u64,
        owners: BTreeMap<String, u64>,
    }
    let windows: Vec<Window> = serde_json::from_slice(&std::fs::read(path)?)?;
    let mut out = String::from("store writes by window and owner (bytes; profiling, not gated)\n");
    for window in windows.iter().filter(|w| w.turn == turn) {
        out.push_str(&format!(
            "{} turn {}: {}\n",
            window.window, window.turn, window.bytes
        ));
        let mut owners: Vec<_> = window.owners.iter().collect();
        owners.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (owner, bytes) in owners {
            out.push_str(&format!("  {owner:<48} {bytes:>10}\n"));
        }
    }
    Ok(out)
}

/// Each page's owner: the table, or `table/index` for an index.
fn page_owners(database: &str) -> Result<BTreeMap<u32, String>> {
    let connection = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {database} for dbstat"))?;
    let mut statement = connection.prepare(
        "SELECT d.pageno, d.name, s.tbl_name FROM dbstat AS d \
         LEFT JOIN sqlite_schema AS s ON s.name = d.name",
    )?;
    let rows = statement.query_map([], |row| {
        let name: String = row.get(1)?;
        let table: Option<String> = row.get(2)?;
        Ok((
            row.get::<_, u32>(0)?,
            match table {
                Some(table) if table != name => format!("{table}/{name}"),
                _ => name,
            },
        ))
    })?;
    rows.collect::<rusqlite::Result<_>>()
        .context("read dbstat page owners")
}

fn pg_lsn(position: u64) -> String {
    format!("{:X}/{:X}", position >> 32, position & 0xFFFF_FFFF)
}

/// WAL record bytes in [$1, $2) by the relation of each record's first block
/// (`table/index` for an index, `table/toast` for a TOAST table); records
/// without a block are named by resource manager.
const PG_OWNERS: &str = "\
WITH relations AS (
    SELECT pg_relation_filenode(c.oid) AS node,
           CASE WHEN i.indrelid IS NOT NULL THEN i.indrelid::regclass::text || '/' || c.relname
                WHEN c.relkind = 't' THEN coalesce(
                    (SELECT t.oid::regclass::text || '/toast' FROM pg_class AS t
                     WHERE t.reltoastrelid = c.oid), c.relname)
                ELSE c.oid::regclass::text END AS owner
    FROM pg_class AS c LEFT JOIN pg_index AS i ON i.indexrelid = c.oid
    WHERE pg_relation_filenode(c.oid) IS NOT NULL),
records AS (
    SELECT record_length, resource_manager,
           substring(block_ref FROM 'rel \\d+/\\d+/(\\d+)')::oid AS node
    FROM pg_get_wal_records_info($1::pg_lsn, $2::pg_lsn))
SELECT coalesce(r.owner, CASE WHEN w.node IS NULL THEN '<' || w.resource_manager || '>'
                              ELSE 'relfilenode ' || w.node END),
       sum(w.record_length)::bigint
FROM records AS w LEFT JOIN relations AS r ON r.node = w.node
GROUP BY 1";
