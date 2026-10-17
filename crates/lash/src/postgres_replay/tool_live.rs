//! Independent tool event class in an owner's existing replay schema and pool.
use super::Bells;
use futures_util::StreamExt;
use lash_core::{
    ExecutionOwner, ToolLiveCursor, ToolLiveError, ToolLiveGapReason, ToolLiveItem,
    ToolLiveReadFault, ToolLiveRecord, ToolLiveReplay, ToolLiveRetention, ToolLiveSubscription,
};
use lash_protocol_types::sync::MutexExt;
use sqlx::Row;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub(crate) struct ToolClass {
    pool: sqlx::PgPool,
    head: String,
    log: String,
    channel: String,
    bells: Bells<String>,
    retention: Arc<Mutex<ToolLiveRetention>>,
}
impl ToolClass {
    pub(crate) fn new(pool: sqlx::PgPool, schema: &str, prefix: &str, channel: String) -> Self {
        let schema = schema.replace('"', "\"\"");
        Self {
            pool,
            head: format!("\"{schema}\".{prefix}_tool_head"),
            log: format!("\"{schema}\".{prefix}_tool_log"),
            channel,
            bells: Bells::default(),
            retention: Arc::new(Mutex::new(ToolLiveRetention::default())),
        }
    }
    pub(crate) fn retention(&self, retention: ToolLiveRetention) {
        *self.retention.lock_recover() = retention;
    }
    /// Reuses the store's confirmed listener; notifications carry no facts.
    pub(crate) fn notification(&self, payload: Option<&str>) -> bool {
        match payload {
            Some(payload) if payload.starts_with("tool-live:") => {
                self.bells.ring([payload[10..].to_owned()]);
                true
            }
            None => {
                self.bells.ring_all();
                false
            }
            _ => false,
        }
    }
    pub(crate) async fn cleanup(&self) -> Result<(), ToolLiveError> {
        let retention = *self.retention.lock_recover();
        sqlx::query(&format!(
            "DELETE FROM {} WHERE published_at < clock_timestamp() - $1 * interval '1 microsecond'",
            self.log
        ))
        .bind(micros(retention))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        // Idle heads and their rows are reclaimed together; recreating a head
        // mints a new class incarnation, so old cursors report Unavailable.
        let mut tx = self.pool.begin().await.map_err(db)?;
        let removed = sqlx::query(&format!("DELETE FROM {} WHERE owner IN (SELECT owner FROM {} ORDER BY touched_at DESC OFFSET $1) OR touched_at < clock_timestamp() - $2 * interval '1 microsecond' RETURNING owner", self.head, self.head))
            .bind(i64::try_from(retention.owners()).unwrap_or(i64::MAX)).bind(micros(retention))
            .fetch_all(&mut *tx).await.map_err(db)?;
        for row in removed {
            let owner: String = row.get("owner");
            sqlx::query(&format!("DELETE FROM {} WHERE owner=$1", self.log))
                .bind(&owner)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            self.bells.ring([owner]);
        }
        tx.commit().await.map_err(db)
    }
    async fn ensure(&self, owner: &str) -> Result<(), ToolLiveError> {
        sqlx::query(&format!("INSERT INTO {} (owner, incarnation, tail, touched_at) VALUES ($1,$2,0,clock_timestamp()) ON CONFLICT DO NOTHING", self.head))
            .bind(owner).bind(uuid::Uuid::new_v4().to_string()).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }
    async fn read(
        &self,
        owner: &ExecutionOwner,
        after: Option<&ToolLiveCursor>,
    ) -> Result<Vec<ToolLiveItem>, ToolLiveError> {
        let key = key(owner)?;
        let retention = *self.retention.lock_recover();
        self.ensure(&key).await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        // A shared head lock makes the head and retained rows one window:
        // publication/cleanup cannot trim between these two reads.
        let row = sqlx::query(&format!(
            "SELECT incarnation, tail FROM {} WHERE owner=$1 FOR SHARE",
            self.head
        ))
        .bind(&key)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        let incarnation: String = row.get("incarnation");
        let tail: i64 = row.get("tail");
        let rows = sqlx::query(&format!("SELECT position, payload FROM {} WHERE owner=$1 AND published_at >= clock_timestamp() - $2 * interval '1 microsecond' ORDER BY position", self.log))
            .bind(&key).bind(micros(retention)).fetch_all(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        let floor = rows
            .first()
            .map_or(tail, |row| row.get::<i64, _>("position") - 1);
        let cursor = |position: i64| ToolLiveCursor {
            owner: owner.clone(),
            incarnation: incarnation.clone(),
            position: u64::try_from(position).unwrap_or(0),
        };
        let mut items = Vec::new();
        let position = match after {
            Some(after)
                if after.incarnation != incarnation
                    || after.position > u64::try_from(tail).unwrap_or(0) =>
            {
                items.push(ToolLiveItem::Gap {
                    cursor: cursor(floor),
                    reason: ToolLiveGapReason::Unavailable,
                });
                floor
            }
            Some(after) if after.position < u64::try_from(floor).unwrap_or(0) => {
                items.push(ToolLiveItem::Gap {
                    cursor: cursor(floor),
                    reason: ToolLiveGapReason::Trimmed,
                });
                floor
            }
            Some(after) => i64::try_from(after.position).unwrap_or(i64::MAX),
            None => {
                if floor != 0 {
                    items.push(ToolLiveItem::Gap {
                        cursor: cursor(floor),
                        reason: ToolLiveGapReason::Trimmed,
                    });
                }
                floor
            }
        };
        for row in rows {
            let at: i64 = row.get("position");
            if at > position {
                let payload: Vec<u8> = row.get("payload");
                // One undecodable row is that item's fault; the feed goes on.
                items.push(match serde_json::from_slice::<ToolLiveRecord>(&payload) {
                    Ok(record) => record.into_item(cursor(at)),
                    Err(error) => ToolLiveItem::Fault {
                        cursor: cursor(at),
                        fault: ToolLiveReadFault::CorruptPayload {
                            detail: error.to_string(),
                        },
                    },
                });
            }
        }
        Ok(items)
    }
}
fn key(owner: &ExecutionOwner) -> Result<String, ToolLiveError> {
    serde_json::to_string(owner).map_err(ToolLiveError::corrupt)
}
fn db(error: sqlx::Error) -> ToolLiveError {
    ToolLiveError::store(error)
}
fn micros(retention: ToolLiveRetention) -> i64 {
    i64::try_from(retention.age().as_micros()).unwrap_or(i64::MAX)
}

#[async_trait::async_trait]
impl ToolLiveReplay for ToolClass {
    async fn publish(
        &self,
        owner: &ExecutionOwner,
        records: Vec<ToolLiveRecord>,
    ) -> Result<(), ToolLiveError> {
        let key = key(owner)?;
        self.ensure(&key).await?;
        let retention = *self.retention.lock_recover();
        let mut tx = self.pool.begin().await.map_err(db)?;
        let row = sqlx::query(&format!(
            "SELECT tail FROM {} WHERE owner=$1 FOR UPDATE",
            self.head
        ))
        .bind(&key)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        let mut tail: i64 = row.get("tail");
        for record in records {
            let payload = serde_json::to_vec(&record).map_err(ToolLiveError::corrupt)?;
            let id = record.id();
            tail = tail
                .checked_add(1)
                .ok_or_else(|| ToolLiveError::store("tool position exhausted"))?;
            sqlx::query(&format!("INSERT INTO {} (owner, position, producer_epoch, ordinal, payload, bytes, published_at) VALUES ($1,$2,$3,$4,$5,$6,clock_timestamp()) ON CONFLICT (owner,producer_epoch,ordinal) DO NOTHING", self.log))
                .bind(&key).bind(tail).bind(&id.producer_epoch).bind(i64::try_from(id.ordinal).map_err(ToolLiveError::corrupt)?)
                .bind(&payload).bind(i64::try_from(payload.len()).unwrap_or(i64::MAX)).execute(&mut *tx).await.map_err(db)?;
        }
        sqlx::query(&format!(
            "UPDATE {} SET tail=$2, touched_at=clock_timestamp() WHERE owner=$1",
            self.head
        ))
        .bind(&key)
        .bind(tail)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(&format!("DELETE FROM {log} WHERE owner=$1 AND position IN (SELECT position FROM (SELECT position, published_at, row_number() OVER (ORDER BY position DESC) AS rank, sum(bytes) OVER (ORDER BY position DESC) AS used FROM {log} WHERE owner=$1) retained WHERE rank>$2 OR used>$3 OR published_at < clock_timestamp() - $4 * interval '1 microsecond')", log=self.log))
            .bind(&key).bind(i64::try_from(retention.events()).unwrap_or(i64::MAX)).bind(i64::try_from(retention.bytes()).unwrap_or(i64::MAX)).bind(micros(retention)).execute(&mut *tx).await.map_err(db)?;
        sqlx::query("SELECT pg_notify($1,$2)")
            .bind(&self.channel)
            .bind(format!("tool-live:{key}"))
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        self.bells.ring([key]);
        Ok(())
    }
    async fn subscribe(
        &self,
        owner: &ExecutionOwner,
        after: Option<ToolLiveCursor>,
    ) -> Result<ToolLiveSubscription, ToolLiveError> {
        if after.as_ref().is_some_and(|cursor| &cursor.owner != owner) {
            return Err(ToolLiveError::WrongOwner);
        }
        let bell = self.bells.register(key(owner)?);
        let queue = VecDeque::from(self.read(owner, after.as_ref()).await?);
        let class = self.clone();
        let owner = owner.clone();
        Ok(futures_util::stream::unfold(
            (bell, queue, after, false),
            move |(mut bell, mut queue, mut after, ended)| {
                let class = class.clone();
                let owner = owner.clone();
                async move {
                    if ended {
                        return None;
                    }
                    loop {
                        if let Some(item) = queue.pop_front() {
                            after = Some(item.cursor().clone());
                            return Some((Ok(item), (bell, queue, after, false)));
                        }
                        if bell.receiver.changed().await.is_err() {
                            return Some((
                                Err(ToolLiveError::NotificationLost),
                                (bell, queue, after, true),
                            ));
                        }
                        match class.read(&owner, after.as_ref()).await {
                            Ok(items) => queue.extend(items),
                            Err(error) => return Some((Err(error), (bell, queue, after, true))),
                        }
                    }
                }
            },
        )
        .boxed())
    }
}
