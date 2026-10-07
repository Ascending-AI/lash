//! The witness ledger's writer, as a node uses it: the outside world.
//!
//! The ledger is a database of its own (`witness.sql`), written under the
//! insert-only `lash_witness_writer` role and stamped by the database's
//! clock. No lash store, transaction or schema touches it, so what it holds
//! is what the bodies did, whatever the nodes' stores say. A body writes its
//! entry before anything else and retries the write until it lands: a body
//! that cannot leave its evidence does not run.

use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};

/// How long a failed witness write waits before it tries again.
const RETRY: Duration = Duration::from_millis(100);

/// How often a held body looks for its release.
const POLL: Duration = Duration::from_millis(50);

/// One node's handle on the witness ledger.
#[derive(Clone, Debug)]
pub struct Witness {
    pool: PgPool,
    node: String,
}

impl Witness {
    /// Connect `node`'s writer to the ledger at `url`. The pool connects
    /// lazily, so a node boots while the server restarts.
    ///
    /// # Errors
    ///
    /// The URL does not parse.
    pub fn connect(url: &str, node: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(2))
            .connect_lazy(url)?;
        Ok(Self {
            pool,
            node: node.to_owned(),
        })
    }

    /// The node this handle writes as.
    #[must_use]
    pub fn node(&self) -> &str {
        &self.node
    }

    /// Record that a body for `call` of `tool` was entered on this node.
    pub async fn entered(&self, call: &str, tool: &str) {
        self.effect(call, tool, "entered").await;
    }

    /// Record that the body for `call` of `tool` returned on this node.
    pub async fn returned(&self, call: &str, tool: &str) {
        self.effect(call, tool, "returned").await;
    }

    async fn effect(&self, call: &str, tool: &str, phase: &str) {
        self.retry(|pool| async move {
            sqlx::query(
                "INSERT INTO witness_effects (call_id, tool, node, phase) VALUES ($1, $2, $3, $4)",
            )
            .bind(call)
            .bind(tool)
            .bind(&self.node)
            .bind(phase)
            .execute(&pool)
            .await
            .map(drop)
        })
        .await;
    }

    /// Record that an attempt of the turn's model call `call` started on
    /// this node, and answer its number: one more than the attempts the
    /// ledger already holds for the call. An attempt writes its entry before
    /// it does anything else, so the ledger counts every attempt the outside
    /// world saw.
    pub async fn model_attempt(&self, call: u32) -> u32 {
        let attempt: i32 = self
            .retry_answer(|pool| async move {
                sqlx::query_scalar(
                    "INSERT INTO witness_model_attempts (call_index, attempt, node) \
                     SELECT $1, COALESCE(MAX(attempt), 0) + 1, $2 \
                     FROM witness_model_attempts WHERE call_index = $1 \
                     RETURNING attempt",
                )
                .bind(i32::try_from(call).unwrap_or(i32::MAX))
                .bind(&self.node)
                .fetch_one(&pool)
                .await
            })
            .await;
        u32::try_from(attempt).unwrap_or(u32::MAX)
    }

    /// Wait until the test records the nemesis marker `release`: a held
    /// body's cue to go on. Read errors (the server restarting) are retried.
    pub async fn released(&self) {
        loop {
            let found: Result<i64, sqlx::Error> =
                sqlx::query_scalar("SELECT count(*) FROM witness_nemesis WHERE kind = 'release'")
                    .fetch_one(&self.pool)
                    .await;
            if matches!(found, Ok(count) if count > 0) {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn retry<F, Fut>(&self, write: F)
    where
        F: Fn(PgPool) -> Fut,
        Fut: std::future::Future<Output = Result<(), sqlx::Error>>,
    {
        self.retry_answer(write).await;
    }

    async fn retry_answer<T, F, Fut>(&self, write: F) -> T
    where
        F: Fn(PgPool) -> Fut,
        Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
    {
        loop {
            match write(self.pool.clone()).await {
                Ok(answer) => return answer,
                Err(error) => {
                    eprintln!("{}: witness write failed, retrying: {error}", self.node);
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    }
}

/// Where a held workload stops and waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// Nothing holds.
    Nothing,
    /// The turn's first model call holds on its first attempt until the
    /// node dies or stops.
    Model,
    /// The turn's first model call holds on its first attempt until the
    /// `release` marker.
    ModelUntilRelease,
    /// `ext.write`'s body holds after its witness entry until the node dies.
    Step,
    /// `ext.write`'s body holds after its witness entry until the `release`
    /// marker, then returns.
    StepUntilRelease,
}

impl Hold {
    /// The hold a node's `LASH_WORKERS_HOLD` names.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "" | "nothing" => Some(Self::Nothing),
            "model" => Some(Self::Model),
            "model-until-release" => Some(Self::ModelUntilRelease),
            "step" => Some(Self::Step),
            "step-until-release" => Some(Self::StepUntilRelease),
            _ => None,
        }
    }

    /// The value that names this hold.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Nothing => "nothing",
            Self::Model => "model",
            Self::ModelUntilRelease => "model-until-release",
            Self::Step => "step",
            Self::StepUntilRelease => "step-until-release",
        }
    }
}
