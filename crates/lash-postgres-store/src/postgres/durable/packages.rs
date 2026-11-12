//! Execution resolution and live-node claim capabilities:
//! the PostgreSQL side of `lash_store_sql::durable::packages`.

use std::sync::LazyLock;

use lash_durable::packages::NodeCapability;
use lash_durable::{DurableError, DurableInstant};
use lash_store_sql::Dialect;
use lash_store_sql::durable::packages::PackageStatements;
use sqlx::PgConnection;

use super::sqlx_failure;

pub(super) static SQL: LazyLock<PackageStatements> =
    LazyLock::new(|| PackageStatements::render(Dialect::postgres()));

/// Every node live at `now`, by node id.
pub(super) async fn live_nodes(
    tx: &mut PgConnection,
    now: DurableInstant,
) -> Result<Vec<NodeCapability>, DurableError> {
    let rows: Vec<lash_durable::packages::NodeRow> =
        crate::observed_sql::query_as(SQL.live_nodes.sql())
            .bind(now.0)
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    lash_durable::packages::nodes_of(rows)
}

/// Up to `limit` actors that have not ended and carry claim requirements,
/// by key after `after`.
pub(super) async fn actor_requirements(
    tx: &mut PgConnection,
    after: Option<&lash_durable::ActorKey>,
    limit: usize,
) -> Result<Vec<(lash_durable::ActorKey, Vec<String>)>, DurableError> {
    let rows: Vec<(String, String)> = crate::observed_sql::query_as(SQL.actor_requirements.sql())
        .bind(after.map_or("", lash_durable::ActorKey::as_str))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    lash_durable::packages::requirement_rows(rows)
}

use super::steps::{Entry, Shape, Step, Ty, Value};
use super::{CommitCapacity, PostgresDurableStore, finish};
use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_durable::packages::PackageRefusal;
use lash_durable::resolutions::{
    AcknowledgedResolution, CommitResolution, ExecutionRegistration, ExecutionResolutions,
    ResolutionKey, ResolutionOwner, ResolutionRecord,
};

async fn load(
    tx: &mut PgConnection,
    key: &ResolutionKey,
) -> Result<Option<ResolutionRecord>, DurableError> {
    let json: Option<String> = crate::observed_sql::query_scalar(SQL.load_resolution.sql())
        .bind(&key.execution.0)
        .bind(key.use_.link.to_string())
        .bind(key.use_.slot.to_string())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    json.map(|json| {
        serde_json::from_str::<ResolutionRecord>(&json)
            .map_err(|_| super::corrupt("resolution", &json))
            .and_then(|record| {
                if record.key == *key {
                    lash_durable::resolutions::validate_resolution(&record)?;
                    Ok(record)
                } else {
                    Err(super::corrupt("resolution key", &json))
                }
            })
    })
    .transpose()
}

lash_store_sql::statements! {
    /// Lock ownership before classifying a resolution or reading its winner,
    /// and the statements of the registration, adoption and seed programs.
    pub(super) struct ResolutionPostgresStatements @ "durable_resolutions_postgres" {
        owner = "SELECT epoch, state, state_revision, base_requires FROM actors WHERE actor_key = ?1 FOR UPDATE";
        /// Execution `?1`'s current uses from the array `?2` of
        /// `[link, slot, use_json, requested_json]`.
        insert_uses = "INSERT INTO execution_uses (execution, link, slot, use_json, requested)
             SELECT ?1, item ->> 0, item ->> 1, item ->> 2, item ->> 3
             FROM jsonb_array_elements(?2::jsonb) AS declarations(item)";
        /// Execution `?1`'s debt atoms from the array `?2` of
        /// `[link, slot, source, family, token, detail]`.
        insert_debts = "INSERT INTO execution_debts (execution, link, slot, source, family, token, detail)
             SELECT ?1, item ->> 0, item ->> 1, item ->> 2, item ->> 3, item ->> 4, item ->> 5
             FROM jsonb_array_elements(?2::jsonb) AS debts(item)";
        /// Whether referrer `?1`/`?2` ended.
        referrer_fenced = "SELECT EXISTS (
                 SELECT 1 FROM referrer_fences WHERE referrer_kind = ?1 AND referrer_id = ?2
             ) AS fenced";
        /// Execution `?1` is registered to actor `?2` exactly as referrer
        /// `?3`, debt `?4` and the current uses `?5` (an array of
        /// `[link, slot, use_json, ...]`) say, live or ended.
        same_registration = "SELECT 1 FROM package_executions e
             WHERE e.execution = ?1 AND e.actor_key = ?2
               AND e.referrer_json = ?3 AND e.requirements_json = ?4
               AND (SELECT count(*) FROM execution_uses u WHERE u.execution = ?1)
                   = jsonb_array_length(?5::jsonb)
               AND NOT EXISTS (
                   SELECT 1 FROM jsonb_array_elements(?5::jsonb) AS declared(item)
                   WHERE NOT EXISTS (
                       SELECT 1 FROM execution_uses u
                       WHERE u.execution = ?1 AND u.link = item ->> 0 AND u.slot = item ->> 1
                         AND u.use_json = item ->> 2
                   )
               )";
        /// Actor `?2`'s live execution `?1`, registered as referrer `?3`,
        /// may adopt the uses `?4` (an array of `[link, slot, use_json,
        /// requested_json]`): no use it keeps names another request.
        adoptable = "SELECT 1 FROM package_executions e
             WHERE e.execution = ?1 AND e.actor_key = ?2 AND e.ended = 0
               AND e.referrer_json = ?3
               AND NOT EXISTS (
                   SELECT 1 FROM jsonb_array_elements(?4::jsonb) AS successor(item)
                   WHERE EXISTS (
                       SELECT 1 FROM execution_uses u
                       WHERE u.execution = ?1 AND u.link = item ->> 0
                         AND u.slot = item ->> 1 AND u.requested <> item ->> 3
                   )
               )";
        /// Use `?2`/`?3` of actor `?4`'s live execution `?1` requests `?5`.
        use_registered = "SELECT 1 FROM package_executions e
             JOIN execution_uses u ON u.execution = e.execution
             WHERE e.execution = ?1 AND u.link = ?2 AND u.slot = ?3
               AND e.ended = 0 AND e.actor_key = ?4 AND u.requested = ?5";
        /// Lock execution `?1`'s referrer, as `lock_xact_by_text` locks the
        /// key `crate::artifact_store::referrer_lock_key` spells.
        lock_execution_referrer = "SELECT pg_advisory_xact_lock(hashtextextended(
                 'lash-artifact-referrer:' || referrer_kind || ':' || referrer_id, 0))
             FROM package_executions WHERE execution = ?1";
        /// Whether execution `?1`'s referrer ended.
        execution_fenced = "SELECT EXISTS (
                 SELECT 1 FROM package_executions e
                 JOIN referrer_fences f
                     ON f.referrer_kind = e.referrer_kind AND f.referrer_id = e.referrer_id
                 WHERE e.execution = ?1
             ) AS fenced";
        /// Seed winner `?4` of use `?2`/`?3` of execution `?1`, unless one won.
        seed_resolution = "INSERT INTO execution_resolutions (execution, link, slot, record_json)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT (execution, link, slot) DO NOTHING";
        /// The winner of use `?2`/`?3` of execution `?1` is `?4`.
        same_resolution = "SELECT 1 FROM execution_resolutions
             WHERE execution = ?1 AND link = ?2 AND slot = ?3 AND record_json = ?4";
        /// A created process's execution writes, as [`REGISTRATION_FUNCTION`]
        /// runs them: registry `?1`, actor `?2`, the step programs `?3`, and
        /// whether the actor's requirements are stamped after them (`?4`).
        register_process_execution = "SELECT o_program, o_step
             FROM lash_register_process_execution(?1, ?2, ?3::JSONB, ?4)";
    }
}

/// The server function a created process's execution writes run in.
pub(super) const REGISTRATION_FUNCTION: &str = "lash_register_process_execution";

/// The entries of the registration and seed programs, which
/// [`REGISTRATION_FUNCTION`] dispatches.
pub(super) fn registration_entries() -> [&'static Entry; 11] {
    [
        &super::turns::LOCK_KEY,
        &REFERRER_FENCED,
        &REGISTER,
        &INSERT_USES,
        &INSERT_DEBTS,
        &SAME_REGISTRATION,
        &USE_REGISTERED,
        &LOCK_EXECUTION_REFERRER,
        &EXECUTION_FENCED,
        &SEED,
        &SAME_RESOLUTION,
    ]
}
pub(super) static POSTGRES: LazyLock<ResolutionPostgresStatements> =
    LazyLock::new(|| ResolutionPostgresStatements::render(Dialect::postgres()));

/// A referrer's lock, by its key.
fn lock_referrer(referrer: &ArtifactReferrer) -> Step {
    Step::any(
        &super::turns::LOCK_KEY,
        vec![Value::text(crate::artifact_store::referrer_lock_key(
            referrer,
        ))],
    )
}

/// The capture slot of whether a registration or a seed inserted its row.
const INSERTED: u8 = 40;

pub(super) static INSERT_USES: Entry =
    Entry::new(|| &POSTGRES.insert_uses, &[Ty::Text, Ty::Text], Shape::Exec);
pub(super) static INSERT_DEBTS: Entry = Entry::new(
    || &POSTGRES.insert_debts,
    &[Ty::Text, Ty::Text],
    Shape::Exec,
);
pub(super) static CLEAR_USES: Entry = Entry::new(|| &SQL.clear_uses, &[Ty::Text], Shape::Exec);
pub(super) static CLEAR_DEBTS: Entry = Entry::new(|| &SQL.clear_debts, &[Ty::Text], Shape::Exec);
pub(super) static REFERRER_FENCED: Entry = Entry::new(
    || &POSTGRES.referrer_fenced,
    &[Ty::Text, Ty::Text],
    Shape::Select,
)
.flagged("fenced");
pub(super) static REGISTER: Entry = Entry::new(
    || &SQL.register_execution,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Text],
    Shape::Exec,
);
pub(super) static SAME_REGISTRATION: Entry = Entry::new(
    || &POSTGRES.same_registration,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Text],
    Shape::Select,
);
pub(super) static ADOPTABLE: Entry = Entry::new(
    || &POSTGRES.adoptable,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text],
    Shape::Select,
);
pub(super) static ADOPT: Entry = Entry::new(
    || &SQL.adopt_execution,
    &[Ty::Text, Ty::Text, Ty::Text],
    Shape::Exec,
);
pub(super) static USE_REGISTERED: Entry = Entry::new(
    || &POSTGRES.use_registered,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Text],
    Shape::Select,
);
pub(super) static LOCK_EXECUTION_REFERRER: Entry = Entry::new(
    || &POSTGRES.lock_execution_referrer,
    &[Ty::Text],
    Shape::Select,
);
pub(super) static EXECUTION_FENCED: Entry =
    Entry::new(|| &POSTGRES.execution_fenced, &[Ty::Text], Shape::Select).flagged("fenced");
pub(super) static SEED: Entry = Entry::new(
    || &POSTGRES.seed_resolution,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text],
    Shape::Exec,
);
pub(super) static SAME_RESOLUTION: Entry = Entry::new(
    || &POSTGRES.same_resolution,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text],
    Shape::Select,
);

/// What a step's refusal means.
enum Refusal {
    /// The write named a use or registration the store does not hold so.
    ExecutionUse,
    /// The referrer ended: the one named, or the registration's own.
    ReferrerEnded(Option<ArtifactReferrer>),
    /// Another winner holds the use.
    Immutable(String),
    /// No refusal: the step answers whatever it must.
    None,
}

/// A registration, adoption or seed as a program with each step's refusal.
type Program = Vec<(Step, Refusal)>;

/// Actor `actor` registers `record`: under its referrer's lock, unless the
/// referrer ended, the registration is inserted with its uses and decoded
/// debt once, and replay must name exactly what is stored.
fn register_program(
    actor: &lash_durable::ActorKey,
    record: &ExecutionRegistration,
) -> Result<Program, DurableError> {
    let rows = record.storage_rows()?;
    let execution = || Value::text(record.execution.0.clone());
    Ok(vec![
        (lock_referrer(&record.referrer), Refusal::None),
        (
            Step::none(
                &REFERRER_FENCED,
                vec![
                    Value::text(rows.referrer_kind),
                    Value::text(rows.referrer_id.clone()),
                ],
            ),
            Refusal::ReferrerEnded(Some(record.referrer.clone())),
        ),
        (
            Step::any(
                &REGISTER,
                vec![
                    execution(),
                    Value::text(actor.as_str()),
                    Value::text(rows.referrer.clone()),
                    Value::text(rows.requirements.clone()),
                    Value::text(rows.referrer_kind),
                    Value::text(rows.referrer_id),
                ],
            )
            .capturing(INSERTED),
            Refusal::None,
        ),
        (
            Step::any(
                &INSERT_USES,
                vec![execution(), Value::text(rows.uses.clone())],
            )
            .when_row(INSERTED, true),
            Refusal::None,
        ),
        (
            Step::any(&INSERT_DEBTS, vec![execution(), Value::text(rows.debts)])
                .when_row(INSERTED, true),
            Refusal::None,
        ),
        (
            Step::one(
                &SAME_REGISTRATION,
                vec![
                    execution(),
                    Value::text(actor.as_str()),
                    Value::text(rows.referrer),
                    Value::text(rows.requirements),
                    Value::text(rows.uses),
                ],
            ),
            Refusal::ExecutionUse,
        ),
    ])
}

/// Actor `actor`'s live execution adopts `successor`'s uses and stable
/// debt, its referrer unchanged and every use it keeps naming the same
/// request; historical winners stay in their own rows.
fn adopt_program(
    actor: &lash_durable::ActorKey,
    successor: &ExecutionRegistration,
) -> Result<Program, DurableError> {
    let rows = successor.storage_rows()?;
    let execution = || Value::text(successor.execution.0.clone());
    Ok(vec![
        (
            Step::one(
                &ADOPTABLE,
                vec![
                    execution(),
                    Value::text(actor.as_str()),
                    Value::text(rows.referrer),
                    Value::text(rows.uses.clone()),
                ],
            ),
            Refusal::ExecutionUse,
        ),
        (lock_referrer(&successor.referrer), Refusal::None),
        (
            Step::none(
                &REFERRER_FENCED,
                vec![
                    Value::text(rows.referrer_kind),
                    Value::text(rows.referrer_id),
                ],
            ),
            Refusal::ReferrerEnded(Some(successor.referrer.clone())),
        ),
        (
            Step::one(
                &ADOPT,
                vec![
                    execution(),
                    Value::text(actor.as_str()),
                    Value::text(rows.requirements),
                ],
            ),
            Refusal::ExecutionUse,
        ),
        (Step::any(&CLEAR_USES, vec![execution()]), Refusal::None),
        (
            Step::any(&INSERT_USES, vec![execution(), Value::text(rows.uses)]),
            Refusal::None,
        ),
        (Step::any(&CLEAR_DEBTS, vec![execution()]), Refusal::None),
        (
            Step::any(&INSERT_DEBTS, vec![execution(), Value::text(rows.debts)]),
            Refusal::None,
        ),
    ])
}

/// Actor `actor` seeds `record` for one of its live execution's uses: under
/// the registration's referrer lock, unless it ended, the winner is
/// inserted with its target debt, or must be the one already stored.
fn seed_program(
    actor: &lash_durable::ActorKey,
    record: &ResolutionRecord,
) -> Result<Program, DurableError> {
    lash_durable::resolutions::validate_resolution(record)?;
    let requested = serde_json::to_string(&record.requested)
        .map_err(|error| super::corrupt("resolution", &error.to_string()))?;
    let stored = serde_json::to_string(record)
        .map_err(|error| super::corrupt("resolution", &error.to_string()))?;
    let debts = lash_durable::resolutions::resolution_debts(record)?;
    let key = || {
        vec![
            Value::text(record.key.execution.0.clone()),
            Value::text(record.key.use_.link.to_string()),
            Value::text(record.key.use_.slot.to_string()),
        ]
    };
    let execution = || Value::text(record.key.execution.0.clone());
    let mut registered = key();
    registered.extend([Value::text(actor.as_str()), Value::text(requested)]);
    let mut insert = key();
    insert.push(Value::text(stored.clone()));
    let mut same = key();
    same.push(Value::text(stored));
    Ok(vec![
        (
            Step::one(&USE_REGISTERED, registered),
            Refusal::ExecutionUse,
        ),
        (
            Step::any(&LOCK_EXECUTION_REFERRER, vec![execution()]),
            Refusal::None,
        ),
        (
            Step::none(&EXECUTION_FENCED, vec![execution()]),
            Refusal::ReferrerEnded(None),
        ),
        (Step::any(&SEED, insert).capturing(INSERTED), Refusal::None),
        (
            Step::any(&INSERT_DEBTS, vec![execution(), Value::text(debts)])
                .when_row(INSERTED, true),
            Refusal::None,
        ),
        (
            Step::one(&SAME_RESOLUTION, same).when_row(INSERTED, false),
            Refusal::Immutable(format!("resolution {:?}", record.key)),
        ),
    ])
}

/// The program of an execution write: a registration, an adoption or a
/// seed, each with its steps' refusals.
fn program(
    actor: &lash_durable::ActorKey,
    write: &lash_durable::DomainWrite,
) -> Result<Option<Program>, DurableError> {
    use lash_durable::DomainWrite;
    match write {
        DomainWrite::RegisterExecution(record) => register_program(actor, record).map(Some),
        DomainWrite::AdoptExecution(record) => adopt_program(actor, record).map(Some),
        DomainWrite::SeedResolution(record) => seed_program(actor, record).map(Some),
        _ => Ok(None),
    }
}

/// An execution write's steps, as the one-call commit ships them.
pub(super) fn steps(
    actor: &lash_durable::ActorKey,
    write: &lash_durable::DomainWrite,
) -> Result<Option<Vec<Step>>, DurableError> {
    Ok(program(actor, write)?.map(|program| program.into_iter().map(|(step, _)| step).collect()))
}

/// Run an execution write's program in `tx`, naming the refusal of the
/// step that refused.
pub(super) async fn apply(
    tx: &mut PgConnection,
    context: &super::steps::Context<'_>,
    write: &lash_durable::DomainWrite,
) -> Result<(), DurableError> {
    let Some(program) = program(context.actor, write)? else {
        return Err(super::corrupt(
            "execution write",
            "a write without a program",
        ));
    };
    let (steps, refusals): (Vec<_>, Vec<_>) = program.into_iter().unzip();
    let Some(refused) = super::steps::run(tx, &steps, context).await? else {
        return Ok(());
    };
    Err(refusal(tx, write, refusals, refused.index).await?)
}

/// What the refusal of step `index` of `write`'s program, whose refusals
/// are `refusals`, means.
async fn refusal(
    tx: &mut PgConnection,
    write: &lash_durable::DomainWrite,
    refusals: Vec<Refusal>,
    index: usize,
) -> Result<DurableError, DurableError> {
    Ok(match refusals.into_iter().nth(index) {
        Some(Refusal::ExecutionUse) => PackageRefusal::ExecutionUse.into(),
        Some(Refusal::ReferrerEnded(Some(referrer))) => {
            PackageRefusal::ReferrerEnded(referrer).into()
        }
        Some(Refusal::ReferrerEnded(None)) => {
            let lash_durable::DomainWrite::SeedResolution(record) = write else {
                return Err(super::corrupt("execution write", "an unnamed referrer"));
            };
            let (_, referrer, _, _): (String, String, String, bool) =
                crate::observed_sql::query_as(SQL.registration.sql())
                    .bind(&record.key.execution.0)
                    .fetch_one(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
            PackageRefusal::ReferrerEnded(
                serde_json::from_str(&referrer)
                    .map_err(|_| super::corrupt("execution referrer", &referrer))?,
            )
            .into()
        }
        Some(Refusal::Immutable(detail)) => PackageRefusal::Immutable(detail).into(),
        Some(Refusal::None) | None => {
            super::corrupt("execution write", "an unguarded step refused")
        }
    })
}

/// Stamp actor `actor`'s effective requirements: its base ones (`requested`,
/// which become its base, or those it holds) merged with its executions'
/// debt, which the store merges ([`PackageStatements::debt_tokens`]).
pub(super) async fn stamp_requires(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
    requested: Option<&[String]>,
) -> Result<(), DurableError> {
    let debts: Vec<String> = crate::observed_sql::query_scalar(SQL.debt_tokens.sql())
        .bind(actor.as_str())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    crate::observed_sql::query(super::executor::EXECUTOR_SQL.stamp_requires.sql())
        .bind(actor.as_str())
        .bind(requested.map(lash_durable::encode_requires))
        .bind(debts)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

#[async_trait::async_trait]
impl ExecutionResolutions for PostgresDurableStore {
    async fn requirements(
        &self,
        actor: &lash_durable::ActorKey,
    ) -> Result<lash_durable::resolutions::ClaimRequirements, DurableError> {
        self.within(CommitCapacity::Work, async {
            let mut connection = self.pools.work.acquire().await.map_err(sqlx_failure)?;
            requirements(&mut connection, actor).await
        })
        .await
    }
    async fn load(&self, key: &ResolutionKey) -> Result<Option<ResolutionRecord>, DurableError> {
        self.within(CommitCapacity::Work, async {
            let mut connection = self.pools.work.acquire().await.map_err(sqlx_failure)?;
            load(&mut connection, key).await
        })
        .await
    }
    async fn commit_acknowledged(
        &self,
        owner: &ResolutionOwner,
        candidate: ResolutionRecord,
    ) -> Result<AcknowledgedResolution, DurableError> {
        self.within(CommitCapacity::Work, async {
            let (mut tx, _) = self
                .open(lash_durable::CommitLabel::RESOLUTION_COMMIT)
                .await?;
            let outcome = commit_resolution(&mut tx, owner, candidate).await;
            finish(tx, outcome).await
        })
        .await
    }
}

async fn commit_resolution(
    tx: &mut PgConnection,
    owner: &ResolutionOwner,
    candidate: ResolutionRecord,
) -> Result<AcknowledgedResolution, DurableError> {
    let row: Option<(i64, String, i64, Option<String>)> =
        crate::observed_sql::query_as(POSTGRES.owner.sql())
            .bind(owner.actor.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    let Some((epoch, state, revision, _requires)) = row else {
        return Err(DurableError::OwnershipLost(lash_durable::Fenced {
            actor: owner.actor.clone(),
            held: owner.epoch,
            current: None,
        }));
    };
    if epoch != owner.epoch.0 || state != "owned" {
        return Err(DurableError::OwnershipLost(lash_durable::Fenced {
            actor: owner.actor.clone(),
            held: owner.epoch,
            current: Some(lash_durable::Epoch(epoch)),
        }));
    }
    let referrer = registered_use(tx, &owner.actor, &candidate).await?;
    crate::artifact_store::lock_referrer_tx(tx, &referrer)
        .await
        .map_err(sqlx_failure)?;
    if crate::artifact_store::is_fenced_tx(tx, &referrer)
        .await
        .map_err(DurableError::from)?
    {
        return Err(lash_durable::packages::PackageRefusal::ReferrerEnded(referrer).into());
    }
    if revision != owner.expected_revision.0 {
        return Err(DurableError::ResolutionRevisionConflict {
            actor: owner.actor.clone(),
            expected: owner.expected_revision,
            current: lash_durable::StateRevision(revision),
        });
    }
    let fence = crate::observed_sql::query(SQL.resolution_fence.sql())
        .bind(owner.actor.as_str())
        .bind(owner.epoch.0)
        .bind(revision)
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let revision = super::get::<i64>(&fence, 0)?;
    let process_view =
        super::process_view::refresh_view(tx, &owner.actor, owner.process_view.clone()).await?;
    let process_views =
        lash_durable::domain::processes::ProcessPostimages::from_view(process_view).into_views();
    let turn_cancel = super::turns::current_cancel(tx, &owner.actor).await?;
    let acknowledge = |result| AcknowledgedResolution {
        result,
        actor: lash_durable::ActorCommit {
            process_views: process_views.clone(),
            root_leases: Vec::new(),
            session_heads: Vec::new(),
            revision: lash_durable::StateRevision(revision),
            state: lash_durable::ActorState::Owned,
            turn_cancel: turn_cancel.clone(),
        },
    };
    if let Some(winner) = load(tx, &candidate.key).await? {
        return Ok(acknowledge(CommitResolution::Existing(winner)));
    }
    let json = serde_json::to_string(&candidate)
        .map_err(|error| super::corrupt("resolution", &error.to_string()))?;
    crate::observed_sql::query(SQL.insert_resolution.sql())
        .bind(&candidate.key.execution.0)
        .bind(candidate.key.use_.link.to_string())
        .bind(candidate.key.use_.slot.to_string())
        .bind(json)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    crate::observed_sql::query(POSTGRES.insert_debts.sql())
        .bind(&candidate.key.execution.0)
        .bind(lash_durable::resolutions::resolution_debts(&candidate)?)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    stamp_requires(tx, &owner.actor, None).await?;
    Ok(acknowledge(CommitResolution::Inserted(candidate)))
}

/// The committing actor `?2`'s execution `?1` ends.
pub(super) static END_EXECUTION: super::steps::Entry = super::steps::Entry::new(
    || &SQL.end_execution,
    &[super::steps::Ty::Text, super::steps::Ty::Text],
    super::steps::Shape::Exec,
);

/// The ending execution `?1`'s debt, when it is actor `?2`'s.
pub(super) static END_DEBTS: super::steps::Entry = super::steps::Entry::new(
    || &SQL.end_debts,
    &[super::steps::Ty::Text, super::steps::Ty::Text],
    super::steps::Shape::Exec,
);

/// `actor`'s execution `execution` ends, as steps: it must be the actor's,
/// and its debt goes with it.
pub(super) fn end_execution_steps(
    actor: &lash_durable::ActorKey,
    execution: &lash_durable::resolutions::ExecutionId,
) -> Vec<super::steps::Step> {
    let values = || {
        vec![
            super::steps::Value::text(execution.0.clone()),
            super::steps::Value::text(actor.as_str()),
        ]
    };
    vec![
        super::steps::Step::one(&END_EXECUTION, values()),
        super::steps::Step::any(&END_DEBTS, values()),
    ]
}

pub(super) async fn end_execution(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
    execution: &lash_durable::resolutions::ExecutionId,
) -> Result<(), DurableError> {
    let changed = crate::observed_sql::query(SQL.end_execution.sql())
        .bind(&execution.0)
        .bind(actor.as_str())
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
        .rows_affected();
    if changed == 0 {
        return Err(lash_durable::packages::PackageRefusal::ExecutionUse.into());
    }
    crate::observed_sql::query(SQL.end_debts.sql())
        .bind(&execution.0)
        .bind(actor.as_str())
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

/// Actor `actor`'s merged execution debt.
async fn requirements(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
) -> Result<lash_durable::resolutions::ClaimRequirements, DurableError> {
    let tokens: Vec<String> = crate::observed_sql::query_scalar(SQL.debt_tokens.sql())
        .bind(actor.as_str())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(lash_vocabulary::claims::ObligationToken::requirements_of(
        tokens.iter().map(String::as_str),
    ))
}

async fn registered_use(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
    candidate: &ResolutionRecord,
) -> Result<lash_core_store::artifact_referrer::ArtifactReferrer, DurableError> {
    let row: Option<(String, String, String)> =
        crate::observed_sql::query_as(SQL.registration_use.sql())
            .bind(&candidate.key.execution.0)
            .bind(candidate.key.use_.link.to_string())
            .bind(candidate.key.use_.slot.to_string())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    lash_durable::resolutions::validate_current_use(actor, candidate, row)
}

/// Actor creation and inherited resolution debt are one registrar commit.
pub(crate) async fn register_process_execution(
    tx: &mut PgConnection,
    process: &lash_vocabulary::identity::ProcessId,
    execution: Option<&ExecutionRegistration>,
    initial: &[ResolutionRecord],
) -> Result<(), DurableError> {
    let Some(execution) = execution else {
        return if initial.is_empty() {
            Ok(())
        } else {
            Err(lash_durable::packages::PackageRefusal::ExecutionUse.into())
        };
    };
    if execution.execution != lash_durable::resolutions::ExecutionId::process(process)
        || execution.referrer
            != lash_core_store::artifact_referrer::ArtifactReferrer::ProcessRecord(process.clone())
    {
        return Err(lash_durable::packages::PackageRefusal::ExecutionUse.into());
    }
    let actor = lash_durable::ActorKey::process(process.as_str())
        .map_err(|_| lash_durable::packages::PackageRefusal::ExecutionUse)?;
    // The writes in the order the explicit form ran them. A program that
    // cannot be built is that write's answer once those before it ran.
    let writes: Vec<lash_durable::DomainWrite> = std::iter::once(
        lash_durable::DomainWrite::RegisterExecution(execution.clone()),
    )
    .chain(
        initial
            .iter()
            .cloned()
            .map(lash_durable::DomainWrite::SeedResolution),
    )
    .collect();
    let mut programs = Vec::with_capacity(writes.len());
    let mut unbuilt = None;
    for write in &writes {
        match program(&actor, write) {
            Ok(Some(program)) => programs.push(program),
            Ok(None) => {
                return Err(super::corrupt(
                    "execution write",
                    "a write without a program",
                ));
            }
            Err(error) => {
                unbuilt = Some(error);
                break;
            }
        }
    }
    if programs.is_empty() {
        return Err(unbuilt.unwrap_or_else(|| super::corrupt("execution write", "no program")));
    }
    let shipped: Vec<Vec<serde_json::Value>> = programs
        .iter()
        .map(|program| {
            program
                .iter()
                .map(|(step, _)| super::executor::step_json(step))
                .collect()
        })
        .collect();
    let (refused_program, refused_step): (Option<i32>, Option<i32>) =
        crate::observed_sql::query_as(POSTGRES.register_process_execution.sql())
            .bind(super::executor::REGISTRY.as_str())
            .bind(actor.as_str())
            .bind(sqlx::types::Json(shipped))
            .bind(unbuilt.is_none())
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    if let (Some(index), Some(step)) = (refused_program, refused_step) {
        let index = usize::try_from(index)
            .map_err(|_| super::corrupt("execution write", "a negative program"))?;
        let step = usize::try_from(step)
            .map_err(|_| super::corrupt("execution write", "a negative step"))?;
        let (Some(write), Some(program)) = (writes.get(index), programs.into_iter().nth(index))
        else {
            return Err(super::corrupt(
                "execution write",
                "an unknown program refused",
            ));
        };
        let refusals = program.into_iter().map(|(_, refusal)| refusal).collect();
        return Err(refusal(tx, write, refusals, step).await?);
    }
    unbuilt.map_or(Ok(()), Err)
}

pub(super) async fn end_actor_executions(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
) -> Result<(), DurableError> {
    crate::observed_sql::query(SQL.end_actor_debts.sql())
        .bind(actor.as_str())
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    crate::observed_sql::query(SQL.end_actor_executions.sql())
        .bind(actor.as_str())
        .execute(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}
