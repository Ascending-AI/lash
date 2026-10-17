//! The PostgreSQL durable domain read port.
use std::future::Future;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;

use sqlx::pool::PoolConnection;
use sqlx::{PgConnection, Postgres};

use super::{
    PostgresDurableStore, processes, prompts, session_close, session_mail, snapshots, sqlx_failure,
    turns, waits,
};
use lash_durable::domain::{
    ExecKey, ExecRootRow, HydratedGraph, OwnerKey, ProcessActorRow, RootOwner, RunFacts, ScopeKey,
    SessionCloseRow, TurnRow, WaitId, WaitRow,
};
use lash_durable::{ActorKey, CommitCapacity, DurableError, DurableReads};

#[async_trait::async_trait]
impl DurableReads for PostgresDurableStore {
    async fn model_call_pins(
        &self,
        session: &lash_sansio::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_durable::domain::ModelCallPinRow>, DurableError> {
        self.read_model_call_pins(session, owner).await
    }

    async fn model_calls(
        &self,
        session: &lash_sansio::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_core_store::model_calls::ModelCallRecord>, DurableError> {
        self.read_model_calls(session, owner).await
    }

    async fn turn(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Option<TurnRow>, DurableError> {
        self.read(|connection| Box::pin(turns::turn(connection, session)))
            .await
    }

    async fn turn_end(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
        self.read(|connection| Box::pin(turns::turn_end(connection, session, run)))
            .await
    }

    async fn turn_namespaces(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
        // The rows and the graphs they root are several statements: one
        // snapshot, so a peel beside the read never leaves it half a graph.
        self.read(|connection| {
            Box::pin(async move {
                let mut snapshot = super::snapshot(connection).await?;
                let namespaces = turns::turn_namespaces(
                    &mut snapshot,
                    session,
                    run,
                    self.pools.graph_reads.owner(),
                )
                .await?;
                snapshot.rollback().await.map_err(super::sqlx_failure)?;
                Ok(namespaces)
            })
        })
        .await
    }

    async fn run_facts(&self, owner: &OwnerKey) -> Result<RunFacts, DurableError> {
        self.read_run_facts(owner, None).await
    }

    async fn run_facts_of(
        &self,
        owner: &OwnerKey,
        run: lash_durable::domain::RunSeq,
    ) -> Result<RunFacts, DurableError> {
        self.read_run_facts(owner, Some(run)).await
    }

    async fn run_facts_within(
        &self,
        owner: &OwnerKey,
        run: Option<lash_durable::domain::RunSeq>,
        max_rows: usize,
        limits: lash_core_store::checkpoint_graph::DecodeLimits,
    ) -> Result<RunFacts, DurableError> {
        self.read_run_facts_within(owner, run, Some(max_rows), limits)
            .await
    }

    async fn exec_root(&self, exec: &ExecKey) -> Result<Option<ExecRootRow>, DurableError> {
        self.read(|connection| Box::pin(snapshots::read(connection, exec)))
            .await
    }

    async fn hydrate(
        &self,
        owner: RootOwner,
        expected: Option<lash_core_store::checkpoint_graph::NodeId>,
        limits: lash_core_store::checkpoint_graph::DecodeLimits,
    ) -> Result<HydratedGraph, DurableError> {
        self.hydrate_owner(owner, expected, limits).await
    }

    async fn cell_roots(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        let prefix = ExecKey::turn_cells(session, run);
        self.read(|connection| Box::pin(snapshots::under(connection, &prefix)))
            .await
    }

    async fn cell_roots_within(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
        max_rows: usize,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        let prefix = ExecKey::turn_cells(session, run);
        self.read(|connection| {
            Box::pin(snapshots::under_limited(
                connection,
                &prefix,
                Some(max_rows),
            ))
        })
        .await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        self.read(|connection| Box::pin(waits::pending(connection, owner)))
            .await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        self.read(|connection| Box::pin(waits::wait(connection, id)))
            .await
    }

    async fn process(
        &self,
        process: &lash_sansio::ProcessId,
    ) -> Result<Option<ProcessActorRow>, DurableError> {
        self.read(|connection| Box::pin(processes::process(connection, process)))
            .await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<lash_sansio::ProcessId>, DurableError> {
        self.read(|connection| {
            Box::pin(processes::live_until_descendants(connection, scope, limit))
        })
        .await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&lash_sansio::ProcessId>,
        limit: usize,
    ) -> Result<Vec<lash_sansio::ProcessId>, DurableError> {
        self.read(|connection| Box::pin(processes::until_children(connection, scope, after, limit)))
            .await
    }

    async fn session_close(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        self.read(|connection| Box::pin(session_close::read(connection, session)))
            .await
    }

    async fn ending_scopes(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Vec<ScopeKey>, DurableError> {
        self.read(|connection| Box::pin(session_close::ending_scopes(connection, session)))
            .await
    }

    async fn session_mailbox(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<lash_durable::domain::SessionMailbox, DurableError> {
        self.read(|connection| Box::pin(session_mail::read(connection, session)))
            .await
    }

    async fn prompt_snapshot(
        &self,
        call: &lash_durable::domain::PromptCallKey,
    ) -> Result<Option<lash_durable::domain::PromptSnapshotRow>, DurableError> {
        self.read(|connection| Box::pin(prompts::snapshot(connection, call)))
            .await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<lash_durable::domain::PromptText>, DurableError> {
        self.read(|connection| Box::pin(prompts::texts(connection, hashes)))
            .await
    }
}

/// A read's body: its statements on the connection it is handed.
pub(super) type ReadBody<'c, T> =
    Pin<Box<dyn Future<Output = Result<T, DurableError>> + Send + 'c>>;

/// The work connection one unfenced read runs on, for as long as the read
/// borrows `'a`.
pub(super) struct ReadConnection<'a> {
    connection: PoolConnection<Postgres>,
    read: PhantomData<&'a ()>,
}

impl Deref for ReadConnection<'_> {
    type Target = PgConnection;

    fn deref(&self) -> &PgConnection {
        &self.connection
    }
}

impl DerefMut for ReadConnection<'_> {
    fn deref_mut(&mut self) -> &mut PgConnection {
        &mut self.connection
    }
}

impl PostgresDurableStore {
    /// Run `body`, an unfenced read, on a work connection under the work
    /// capacity's admission and deadline.
    ///
    /// A checkout pings only a connection idle for 10 s (FIG-5936), so a
    /// connection the server closed while it sat in the pool, as a restart
    /// closes every one, is first met by a read's own statement (FIG-5975).
    /// A read writes nothing: one that failed on a connection that no longer
    /// answers a ping runs once more, on a connection that does. A failure
    /// on a live connection is the read's answer.
    pub(super) async fn read<'a, T>(
        &self,
        body: impl for<'c> Fn(&'c mut ReadConnection<'a>) -> ReadBody<'c, T> + Send + Sync,
    ) -> Result<T, DurableError> {
        self.within(CommitCapacity::Work, async {
            let mut connection = ReadConnection {
                connection: self.reader().await?,
                read: PhantomData,
            };
            let failure = match body(&mut connection).await {
                Ok(value) => return Ok(value),
                Err(failure) => failure,
            };
            if sqlx::Connection::ping(&mut *connection).await.is_ok() {
                return Err(failure);
            }
            connection.connection.close_on_drop();
            drop(connection);
            let mut connection = ReadConnection {
                connection: crate::observed_sql::verified(&self.pools.work)
                    .await
                    .map_err(sqlx_failure)?,
                read: PhantomData,
            };
            body(&mut connection).await
        })
        .await
    }
}
