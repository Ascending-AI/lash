//! VM snapshots on PostgreSQL: the `snapshots` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L7 (FIG-5177). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use std::sync::LazyLock;

use lash_durable::domain::{DomainRefusal, ExecKey, SnapshotRev, SnapshotRow, SnapshotWrite};
use lash_durable::{DurableError, Epoch};
use lash_store_sql::Dialect;
use lash_store_sql::durable::snapshots::SnapshotStatements;
use sqlx::{PgConnection, Row};

use super::{Committing, integer, sqlx_failure};

static SQL: LazyLock<SnapshotStatements> =
    LazyLock::new(|| SnapshotStatements::render(Dialect::postgres()));

fn revision(stored: i64) -> Result<SnapshotRev, DurableError> {
    integer(stored).map(SnapshotRev)
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &SnapshotWrite,
) -> Result<(), DurableError> {
    match write {
        SnapshotWrite::Put {
            exec,
            expected,
            snapshot_ref,
            executable_identity,
            format_version,
        } => {
            let key = exec.stored();
            let found: Option<i64> = sqlx::query_scalar(SQL.rev.sql())
                .bind(&key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            let found = found.map(revision).transpose()?;
            if found != *expected {
                return Err(DurableError::Domain(DomainRefusal::SnapshotRevConflict {
                    exec: exec.clone(),
                    expected: *expected,
                    found,
                }));
            }
            let format_version = integer::<i64>(*format_version)?;
            match expected {
                None => {
                    sqlx::query(SQL.insert.sql())
                        .bind(&key)
                        .bind(snapshot_ref)
                        .bind(executable_identity)
                        .bind(format_version)
                        .bind(commit.epoch.0)
                        .execute(&mut *tx)
                        .await
                        .map_err(sqlx_failure)?;
                }
                Some(expected) => {
                    sqlx::query(SQL.replace.sql())
                        .bind(&key)
                        .bind(integer::<i64>(expected.0)?)
                        .bind(snapshot_ref)
                        .bind(executable_identity)
                        .bind(format_version)
                        .bind(commit.epoch.0)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(sqlx_failure)?;
                }
            }
            Ok(())
        }
        SnapshotWrite::Delete { exec } => {
            sqlx::query(SQL.delete.sql())
                .bind(exec.stored())
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
    }
}

pub(super) async fn read(
    tx: &mut PgConnection,
    exec: &ExecKey,
) -> Result<Option<SnapshotRow>, DurableError> {
    let Some(row) = sqlx::query(SQL.read.sql())
        .bind(exec.stored())
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let format_version: i64 = row.try_get(3).map_err(sqlx_failure)?;
    Ok(Some(SnapshotRow {
        exec: exec.clone(),
        rev: revision(row.try_get(0).map_err(sqlx_failure)?)?,
        snapshot_ref: row.try_get(1).map_err(sqlx_failure)?,
        executable_identity: row.try_get(2).map_err(sqlx_failure)?,
        format_version: integer::<u32>(format_version)?,
        written_epoch: Epoch(row.try_get(4).map_err(sqlx_failure)?),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_durable::domain::CellId;
    use lash_durable::{ActorKey, DurableInstant};
    use sqlx::Connection as _;

    #[tokio::test]
    async fn snapshots_preserve_full_format_version_and_refuse_oversized_revision() {
        let url = crate::testing::required_database_url();
        let database = crate::testing::IsolatedDatabase::create(&url).await;
        let mut conn = sqlx::PgConnection::connect(database.url())
            .await
            .expect("open snapshots");
        sqlx::raw_sql(crate::PostgresStorage::schema_ddl())
            .execute(&mut conn)
            .await
            .expect("create snapshots");
        let actor = ActorKey::session("session").expect("actor");
        let commit = Committing {
            actor: &actor,
            epoch: Epoch(1),
            now: DurableInstant(1),
            fleet: lash_core_execution::FleetFormat::current(),
        };
        let exec = ExecKey::Cell("session".into(), "turn".into(), CellId::new("cell"));
        let put = |expected| SnapshotWrite::Put {
            exec: exec.clone(),
            expected,
            snapshot_ref: "snapshot".into(),
            executable_identity: "identity".into(),
            format_version: u32::MAX,
        };
        apply(&mut conn, &commit, &put(None))
            .await
            .expect("insert snapshot");
        let before = read(&mut conn, &exec)
            .await
            .expect("read")
            .expect("snapshot");
        assert_eq!(before.format_version, u32::MAX);
        assert!(
            apply(&mut conn, &commit, &put(Some(SnapshotRev(u64::MAX))))
                .await
                .is_err()
        );
        assert_eq!(read(&mut conn, &exec).await.expect("read"), Some(before));
    }
}
