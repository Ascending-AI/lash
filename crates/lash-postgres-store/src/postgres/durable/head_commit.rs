//! A session head commit as a step program (FIG-5942): what the session
//! store's shared planner decides over the facts the commit was planned on,
//! written as guarded statements.
//!
//! The planner (`RuntimeCommitPlanner`, shared with SQLite) decides the
//! commit from a few facts: the head revision it publishes over, the
//! published leaf's ancestry seed, whether a requested ancestor is active
//! and which of its nodes are taken. A one-call commit plans over the hints
//! its owner's open read ([`SessionFacts`]), and its program guards every
//! one of them: a hint that moved refuses the program, the one call is
//! declined, and the explicit form plans again and names the refusal.
//!
//! The explicit form plans over the same facts read in its own transaction
//! under the locks it keeps to its commit ([`Held`], FIG-7049): the
//! session's write lock, which every head commit, receipt and deletion of
//! the session takes, and the published leaf's row. Those locks discharge
//! the guards of the facts they hold, so its program runs none of them. The
//! nodes a commit's ids name are not the session's: another session's
//! commit can take an id the lock does not hold, so both forms guard them.
//!
//! Every decision the store's rows make is the statements' own: whether
//! the session's retention releases revisions at a commit, and whether the
//! session has work left, which picks the receipt the commit records, are
//! a step's captured answer. Facts only the explicit form reads (an
//! ancestor's reachability, the ownership of a store writer's head, the
//! rows a cancel released, the cleanups of the frames a commit leaves) are
//! read under locks the commit keeps, and the one call declines a commit
//! that needs them.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use lash_core_store::session_identity::FrameNodeId;
use lash_core_store::store::HeadWriter;
use lash_core_store::store::ParentNodeFacts;
use lash_core_store::store::PublishedLeafFacts;
use lash_core_store::store::RuntimeCommitPlan;
use lash_core_store::store::RuntimeCommitPlanner;
use lash_core_store::store::RuntimeCommitReceipt;
use lash_core_store::store::StoreError;
use lash_durable::{ActorKey, DurableInstant};
use lash_store_sql::Dialect;
use lash_vocabulary::identity::NodeId;
use sqlx::Row as _;

use super::steps::{Collect, Entry, Shape, Step, Ty, Value};
use crate::session_sql::session_sql;

lash_store_sql::statements! {
    /// The head commit's own guards.
    pub(crate) struct HeadCommitStatements @ "durable_head_commit" {
        /// Lock session `?1`'s head, answering whether it is still at
        /// revision `?2`.
        head_at = "SELECT head_revision = ?2 AS current FROM session_head
             WHERE session_id = ?1 FOR UPDATE";

        /// Record session `?1`'s commit at `?2`, answering whether its
        /// retention releases revisions at a commit (`until_gc` does not).
        touch = "UPDATE session_meta SET last_commit_at_ms = ?2 WHERE session_id = ?1
             RETURNING retention_kind <> 'until_gc' AS releases";

        /// The plugins among `?1` whose recorded writer range is still
        /// `?2` to `?3`.
        plugin_writers_held = "SELECT plugin_id FROM fleet_plugin_writers
             WHERE (plugin_id, min_format, max_format) IN (
                 SELECT * FROM unnest(?1::TEXT[], ?2::INTEGER[], ?3::INTEGER[]))";

        /// Take attachment `?2`'s fence lock in class `?1`.
        lock_attachment = "SELECT pg_advisory_xact_lock(CAST(?1 AS INTEGER), hashtext(?2))";

        /// Session `?1`'s head as the explicit form plans over it: its
        /// revision's config root, revision, leaf and checkpoint, the frame
        /// its leaf row names, and whether the store holds the config root
        /// as a graph node.
        head = "SELECT revision.config_ref, head.head_revision, revision.leaf_node_id,
                    revision.checkpoint_ref, leaf.frame_node_id, root.encoding = 1 AS root_held
             FROM session_head AS head LEFT JOIN session_revisions AS revision
                 ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
             LEFT JOIN graph_nodes AS leaf ON leaf.node_id = revision.leaf_node_id
             LEFT JOIN blobs AS root ON root.hash = revision.config_ref
             WHERE head.session_id = ?1";
    }
}

static SQL: LazyLock<HeadCommitStatements> =
    LazyLock::new(|| HeadCommitStatements::render(Dialect::postgres()));

fn attachments() -> &'static crate::attachments::AttachmentSql {
    crate::attachments::attachment_sql()
}

fn ingress() -> &'static crate::turn_ingress::TurnIngressSql {
    crate::turn_ingress::turn_ingress_sql()
}

pub(super) static LOCK_SESSION: Entry = Entry::new(
    || &super::SQL.postgres.lock_session_writes,
    &[Ty::Text, Ty::Text],
    Shape::Select,
);
pub(super) static DELETED: Entry = Entry::new(
    || &session_sql().deleted_postgres.exists,
    &[Ty::Text],
    Shape::Select,
)
.flagged("exists");
pub(super) static MATERIALIZED: Entry = Entry::new(
    || &session_sql().meta_postgres.exists_materialized,
    &[Ty::Text],
    Shape::Select,
)
.flagged("exists");
pub(super) static RECEIPT: Entry = Entry::new(
    || &session_sql().turn_commits.select_receipt,
    &[Ty::Text, Ty::Text],
    Shape::Select,
);
pub(super) static PLUGIN_WRITERS: Entry = Entry::new(
    || &SQL.plugin_writers_held,
    &[Ty::Texts, Ty::Ints, Ty::Ints],
    Shape::Select,
);
pub(super) static HEAD_AT: Entry =
    Entry::new(|| &SQL.head_at, &[Ty::Text, Ty::Int], Shape::Select).flagged("current");
pub(super) static HEAD_ABSENT: Entry = Entry::new(
    || &session_sql().head_postgres.select_revision_for_update,
    &[Ty::Text],
    Shape::Select,
);
pub(super) static LEAF: Entry = Entry::new(
    || &session_sql().graph_postgres.select_parent_facts_for_update,
    &[Ty::Text],
    Shape::Select,
);
pub(super) static OCCUPIED: Entry = Entry::new(
    || &session_sql().graph_postgres.select_occupied,
    &[Ty::Texts],
    Shape::Select,
);
pub(super) static NODES: Entry = Entry::new(
    || &session_sql().graph_postgres.insert_batch,
    &[
        Ty::Text,
        Ty::Texts,
        Ty::Texts,
        Ty::Ints,
        Ty::Texts,
        Ty::Ints,
        Ty::Texts,
    ],
    Shape::Returning,
);
pub(super) static REVISION: Entry = Entry::new(
    || &session_sql().revisions.insert,
    &[Ty::Text, Ty::Int, Ty::Text, Ty::Key, Ty::Key],
    Shape::Exec,
);
pub(super) static HEAD_CAS: Entry = Entry::new(
    || &session_sql().head_postgres.upsert_cas,
    &[Ty::Text, Ty::Int, Ty::Int],
    Shape::Exec,
);
pub(super) static TOUCH: Entry = Entry::new(|| &SQL.touch, &[Ty::Text, Ty::Now], Shape::Returning);
/// The revisions a session's retention releases, the ancestry only they
/// kept alive and the checkpoint and head document roots they published.
pub(super) static PRUNE: Entry = Entry::new(
    || &session_sql().revisions.prune_unretained,
    &[Ty::Int, Ty::Text],
    Shape::Returning,
)
.collecting(Collect::Ancestry("leaf_node_id"))
.collecting(Collect::Closure("checkpoint_ref"))
.collecting(Collect::Closure("config_ref"));
pub(super) static SETTLE_COMMAND: Entry = Entry::new(
    || &ingress().queued_batches.settle_command,
    &[Ty::Text, Ty::Text, Ty::Now, Ty::Text, Ty::Text],
    Shape::Exec,
);
pub(super) static SETTLE_INPUT: Entry = Entry::new(
    || &ingress().pending_inputs.settle_admitted,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Now],
    Shape::Exec,
);
pub(super) static RELEASE_INPUT: Entry = Entry::new(
    || &ingress().pending_inputs.release_admitted,
    &[Ty::Text, Ty::Text, Ty::Text],
    Shape::Exec,
);
pub(super) static SETTLE_BATCH: Entry = Entry::new(
    || &ingress().queued_batches.settle_admitted,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Now],
    Shape::Exec,
);
pub(super) static RELEASE_BATCH: Entry = Entry::new(
    || &ingress().queued_batches.release_admitted,
    &[Ty::Text, Ty::Text, Ty::Text],
    Shape::Exec,
);
pub(super) static REFERRER_FENCED: Entry = Entry::new(
    || {
        &crate::artifact_store::artifact_sql()
            .fences
            .select_is_fenced
    },
    &[Ty::Text, Ty::Text],
    Shape::Select,
)
.flagged("exists");
pub(super) static LOCK_ATTACHMENT: Entry =
    Entry::new(|| &SQL.lock_attachment, &[Ty::Int, Ty::Text], Shape::Select);
pub(super) static ATTACHMENT_DELETING: Entry = Entry::new(
    || &attachments().postgres.select_deleting,
    &[Ty::Key],
    Shape::Select,
)
.flagged("exists");
pub(super) static ATTACHMENT_UPLOADED: Entry = Entry::new(
    || &attachments().uploads.select_evidence,
    &[Ty::Key],
    Shape::Select,
);
pub(super) static ATTACHMENT_CLAIMED: Entry = Entry::new(
    || &attachments().condemnation.delete_unclaimed_condemned,
    &[Ty::Key],
    Shape::Exec,
);
pub(super) static ATTACHMENT_EDGE: Entry = Entry::new(
    || &attachments().edges.insert,
    &[Ty::Key, Ty::Text, Ty::Text],
    Shape::Exec,
);
pub(super) static WORK_REMAINING: Entry = Entry::new(
    || &ingress().family.has_admissible_work,
    &[Ty::Text],
    Shape::Select,
);
pub(super) static RECORD_RECEIPT: Entry = Entry::new(
    || &session_sql().turn_commits_postgres.insert_staged,
    &[
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Now,
        Ty::Text,
        Ty::Int,
        Ty::Int,
        Ty::Bool,
        Ty::Int,
    ],
    Shape::Exec,
);
pub(super) static FENCE_REFERRER: Entry = Entry::new(
    || &crate::artifact_store::artifact_sql().fences.insert_fence,
    &[Ty::Text, Ty::Text, Ty::Now],
    Shape::Exec,
);
pub(super) static CLEANUP_INSERT: Entry = Entry::new(
    || &crate::obligation_ledger::cleanup_sql().insert_if_absent,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Now],
    Shape::Exec,
);
pub(super) static CLEANUP_REPLACE: Entry = Entry::new(
    || &crate::obligation_ledger::cleanup_sql().replace_guard_with_ended,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Now],
    Shape::Exec,
);
pub(super) static CARRY_EDGE: Entry = Entry::new(
    || &crate::artifact_store::artifact_sql().edges.insert_edge,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text],
    Shape::Exec,
);

/// The capture slots of a head commit's program.
const RETENTION: u8 = 0;
const WORK: u8 = 1;

/// What a head commit's step refusal means, as the store reports it.
#[derive(Clone, Debug)]
pub(super) enum Refusal {
    /// A guard the plan's facts cannot have moved past: the backstop.
    Backstop,
    Deleted,
    NotFound,
    HeadMoved,
    LeafRetired(NodeId),
    Occupied,
    Collision,
    Command(lash_vocabulary::identity::BatchId),
    /// An ingress settlement always names the run that planned it.
    Ingress {
        run: lash_vocabulary::identity::TurnId,
        row: lash_core_store::store::IngressRowId,
    },
    SessionEnded,
    Attachment(lash_protocol_types::attachment::AttachmentId),
}

/// The facts a head commit is planned on.
#[derive(Clone, Debug)]
pub(crate) struct HeadFacts {
    pub(crate) config_ref: Option<String>,
    /// Whether the session has a head row.
    pub(crate) exists: bool,
    /// The published head's revision: 0 when the session has none.
    pub(crate) revision: u64,
    /// The published leaf, with its ancestry seed when it is live.
    pub(crate) leaf: PublishedLeafFacts,
    /// The frame the published leaf's row names.
    pub(crate) frame: Option<FrameNodeId>,
}

/// What an owner's open read of its session's head (FIG-5942): a hint its
/// one-call commit plans on, never a fact. The call guards each one, and a
/// hint that moved declines the call.
#[derive(Clone, Debug)]
pub(crate) struct SessionFacts {
    pub(crate) head: HeadFacts,
    /// Every recorded plugin writer range.
    pub(crate) plugin_writers: Vec<(String, i32, i32)>,
}

impl SessionFacts {
    /// Decode the facts an owner's open read in `row` from column `first`:
    /// the head revision, the leaf, its generation, frame and liveness, and
    /// the plugin writer ranges as one JSON array.
    pub(super) fn of(row: &sqlx::postgres::PgRow, first: usize) -> Option<Self> {
        let revision: Option<i64> = row.try_get(first).ok()?;
        let leaf: Option<String> = row.try_get(first + 1).ok()?;
        let generation: Option<i64> = row.try_get(first + 2).ok()?;
        let frame: Option<String> = row.try_get(first + 3).ok()?;
        let live: Option<bool> = row.try_get(first + 4).ok()?;
        let writers: Option<serde_json::Value> = row.try_get(first + 5).ok()?;
        let frame = frame.map(FrameNodeId::new).transpose().ok()?;
        let leaf = match leaf {
            None => PublishedLeafFacts::Absent,
            Some(leaf) => {
                let node_id = NodeId::parse(&leaf).ok()?;
                match (live, generation, &frame) {
                    (Some(true), Some(generation), Some(frame)) => {
                        PublishedLeafFacts::Live(ParentNodeFacts {
                            node_id,
                            generation: u64::try_from(generation).ok()?,
                            frame_node_id: NodeId::parse(frame.as_str()).ok()?,
                        })
                    }
                    _ => PublishedLeafFacts::Retired { node_id },
                }
            }
        };
        let plugin_writers = match writers {
            None => Vec::new(),
            Some(writers) => serde_json::from_value(writers).ok()?,
        };
        Some(Self {
            head: HeadFacts {
                config_ref: crate::keys::get(row, first + 6).ok()?,
                exists: revision.is_some(),
                revision: u64::try_from(revision.unwrap_or(0)).ok()?,
                leaf,
                frame,
            },
            plugin_writers,
        })
    }
}

/// The hints of recent opens, by actor and epoch: what the owner's last
/// open read, or what its last one-call head commit published.
#[derive(Debug, Default)]
pub(crate) struct SessionHints {
    known: std::sync::Mutex<KnownHints>,
}

/// A hint's key: its actor and the epoch it is owned at.
type HintKey = (String, i64);

/// The hints kept, and their keys oldest first.
type KnownHints = (
    std::collections::HashMap<HintKey, SessionFacts>,
    std::collections::VecDeque<HintKey>,
);

/// How many opens' hints a storage keeps.
const SESSION_HINTS: usize = 1024;

impl SessionHints {
    pub(super) fn note(&self, key: (String, i64), facts: SessionFacts) {
        let mut known = self
            .known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (map, order) = &mut *known;
        if map.insert(key.clone(), facts).is_none() {
            order.push_back(key);
        }
        while order.len() > SESSION_HINTS {
            if let Some(oldest) = order.pop_front() {
                map.remove(&oldest);
            }
        }
    }

    /// Forget `key`'s hint: a call planned on it was declined, so the
    /// owner's commits take the explicit form until it opens again.
    pub(super) fn forget(&self, key: &(String, i64)) {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .remove(key);
    }

    pub(super) fn get(&self, key: &(String, i64)) -> Option<SessionFacts> {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .get(key)
            .cloned()
    }
}

/// A released or dropped row of a cancel the commit settles, read under its
/// lock: how it is settled and what the outcome reports of it.
#[derive(Clone, Debug)]
pub(super) struct Released {
    row: lash_core_store::store::IngressRowId,
    disposition: lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy,
    affected: Option<(
        u64,
        lash_core_store::turn_control_vocabulary::TurnCancelAffectedInput,
    )>,
}

/// The cleanup a frame the commit leaves arms, decided under its
/// referrer's lock.
#[derive(Clone, Debug)]
struct EndedFrame {
    referrer: lash_core_store::artifact_referrer::ArtifactReferrer,
    decision: lash_core_store::store::CleanupUpsert,
}

/// What the explicit form reads beyond the head's facts, under the locks
/// the commit keeps.
#[derive(Debug, Default)]
pub(super) struct ReadFacts {
    requested_ancestor_is_active: bool,
    occupied: std::collections::HashSet<NodeId>,
    released: Vec<Released>,
    ended: Vec<EndedFrame>,
    /// The `(namespace, artifact_ref)` pairs a frame transition carries.
    carries: Vec<(&'static str, String)>,
}

/// What a head commit's program is planned over, which decides the guards
/// it runs.
pub(super) enum Basis<'a> {
    /// An owner's open read: hints, each guarded by a step.
    Hinted(&'a SessionFacts),
    /// What the explicit form read in its own transaction under the locks it
    /// keeps.
    Held(Box<Held>),
}

/// The facts the explicit form read in its transaction under the locks it
/// keeps to its commit, and the checkpoint it put among them. Only
/// [`read_facts`] makes one, in the transaction its program then runs in: it
/// is never kept past that transaction, so a retry reads afresh.
pub(super) struct Held {
    head: HeadFacts,
    read: ReadFacts,
    checkpoint: crate::support::CheckpointParts,
}

/// A head commit's program: its steps with what each refusal means, the
/// checkpoint the commit puts, and the receipt it answers.
pub(super) struct Program {
    pub(super) steps: Vec<Step>,
    pub(super) refusals: Vec<Refusal>,
    pub(super) checkpoint: crate::support::CheckpointParts,
    pub(super) result: RuntimeCommitReceipt,
    /// The head's facts once the program committed, when the plan names
    /// them.
    pub(super) after: Option<HeadFacts>,
}

impl Program {
    fn push(&mut self, step: Step, refusal: Refusal) {
        self.steps.push(step);
        self.refusals.push(refusal);
    }
}

fn text(value: impl Into<String>) -> Value {
    Value::text(value)
}

fn sql_int(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} does not fit PostgreSQL BIGINT")))
}

/// The receipt row's columns but the result, the instant and the head
/// revision.
fn receipt_values(
    plan: &RuntimeCommitPlan<'_>,
    result: &RuntimeCommitReceipt,
    head_revision: i64,
) -> Result<Vec<Value>, StoreError> {
    let receipt = plan.receipt_write(result);
    let (request_identity_hash, requested_node_count, identity_encoding_version) =
        crate::runtime_persistence::append_identity_columns(receipt.append_request_identity)?;
    Ok(vec![
        Value::text(receipt.session_id.as_str()),
        Value::text(receipt.operation_key),
        Value::text(receipt.turn_commit_hash),
        Value::text(receipt.result.encode_result_json()?),
        Value::Text(
            receipt
                .result
                .outcome
                .as_ref()
                .map(|outcome| outcome.as_str().to_owned()),
        ),
        Value::Now,
        Value::Text(request_identity_hash.map(str::to_owned)),
        Value::Int(requested_node_count),
        Value::Int(identity_encoding_version.map(i64::from)),
        Value::Bool(!receipt.result.failure_evidence.is_empty()),
        Value::int(head_revision),
    ])
}

/// The program of the commit `planner` prepared, planned over `basis`.
/// `Ok(None)`: the commit needs a fact only the explicit form reads.
pub(super) fn program(
    planner: &RuntimeCommitPlanner,
    basis: Basis<'_>,
    now: u64,
) -> Result<Option<Program>, StoreError> {
    let commit = planner.commit();
    let session = commit.session_id.as_str();
    planner.validate_node_derivation()?;
    if commit.run_terminal.is_some() {
        return Err(StoreError::Backend(
            "a PostgreSQL head commit records no run terminal: a run's terminal is its turn's \
             own write"
                .to_owned(),
        ));
    }
    let ancestor = crate::runtime_persistence::requested_append_ancestor(&commit.turn_commit);
    let released_any = commit
        .ingress
        .as_ref()
        .is_some_and(|ingress| !ingress.released.is_empty() || !ingress.dropped.is_empty());
    let unread = ReadFacts {
        requested_ancestor_is_active: true,
        ..ReadFacts::default()
    };
    let held;
    let (facts, read, put, hint) = match basis {
        // A one-call commit plans only what its open's facts decide.
        Basis::Hinted(_)
            if ancestor.is_some() || released_any || commit.frame_transition.is_some() =>
        {
            return Ok(None);
        }
        Basis::Hinted(hint) => (&hint.head, &unread, None, Some(hint)),
        Basis::Held(acquired) => {
            let Held {
                head,
                read,
                checkpoint,
            } = *acquired;
            held = (head, read);
            (&held.0, &held.1, Some(checkpoint), None)
        }
    };
    let plan = planner.plan(lash_core_store::store::FreshRuntimeCommitFacts {
        actual_head_revision: facts.revision,
        published_leaf: facts.leaf.clone(),
        requested_ancestor_is_active: read.requested_ancestor_is_active,
        occupied_node_ids: read.occupied.clone(),
    })?;
    let fleet = plan.fleet_format();
    let next = crate::support::sql_monotonic_counter_value(
        "session_head_revision",
        plan.actual_head_revision(),
        plan.next_head_revision(),
    )?;
    // The explicit form put its checkpoint before it planned.
    let checkpoint = match put {
        Some(put) => put,
        None => crate::support::checkpoint_parts(&commit.checkpoint, fleet)?,
    };
    let meta = plan.head_meta(checkpoint.checkpoint_ref.clone());
    let left = lash_core_store::store::frames_left_by_commit(
        facts.frame.as_ref(),
        &commit.graph,
        meta.current_frame_node_id.as_ref(),
    );
    if !left.is_empty() && read.ended.is_empty() {
        // The cleanups the left frames arm are read under their locks.
        return Ok(None);
    }
    if let Some(transition) = &commit.frame_transition
        && (transition.ended.session_id() != commit.session_id
            || transition.successor.session_id() != commit.session_id
            || !left.contains(transition.ended.frame_node_id())
            || meta.current_frame_node_id.as_ref() != Some(transition.successor.frame_node_id()))
    {
        return Err(StoreError::Backend(
            "frame transition does not match the committed head".into(),
        ));
    }
    let mut affected: Vec<_> = read
        .released
        .iter()
        .filter_map(|released| released.affected.clone())
        .collect();
    affected.sort_by_key(|(enqueue_seq, _)| *enqueue_seq);
    let mut result = plan.result(
        checkpoint.checkpoint_ref.clone(),
        checkpoint.manifest.clone(),
        now,
        false,
    );
    result.turn_cancel_input_outcome =
        lash_core_store::turn_control_vocabulary::TurnCancelInputOutcome {
            affected_inputs: affected.into_iter().map(|(_, input)| input).collect(),
        };
    let after_leaf = match &meta.leaf_node_id {
        None => Some(PublishedLeafFacts::Absent),
        Some(leaf) => match plan
            .planned_node_facts()
            .iter()
            .find(|planned| planned.node_id == *leaf)
        {
            Some(planned) => Some(PublishedLeafFacts::Live(ParentNodeFacts {
                node_id: planned.node_id.clone(),
                generation: planned.generation,
                frame_node_id: planned.frame_node_id.clone(),
            })),
            None => match &facts.leaf {
                PublishedLeafFacts::Live(parent) if parent.node_id == *leaf => {
                    Some(facts.leaf.clone())
                }
                _ => None,
            },
        },
    };
    let mut config_staging = lash_core_store::checkpoint_graph::GraphStaging::new(
        lash_core_store::checkpoint_graph::EncodeBudget::new(
            lash_core_store::checkpoint_graph::ExplicitLimit::new(u64::MAX),
        ),
    );
    let config_root =
        lash_core_store::checkpoint_graph::config::stage(&mut config_staging, &meta.payload())
            .map_err(|message| StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                message,
            })?;
    let config_ref = config_root.to_hex();
    let after = after_leaf.map(|leaf| HeadFacts {
        config_ref: Some(config_ref.clone()),
        exists: true,
        revision: plan.next_head_revision(),
        leaf,
        frame: meta.current_frame_node_id.clone(),
    });
    let mut program = Program {
        steps: Vec::new(),
        refusals: Vec::new(),
        checkpoint,
        result,
        after,
    };

    // The plugin namespaces the commit publishes are admitted against the
    // writer ranges the plan read (FIG-4746); the explicit form admitted
    // them under the fence it holds.
    let publication = planner.plugin_publication();
    if !publication.is_empty()
        && let Some(recorded) = hint.map(|hint| &hint.plugin_writers)
    {
        let plugins = publication.plugins();
        let held: Vec<&(String, i32, i32)> = recorded
            .iter()
            .filter(|(plugin, ..)| plugins.contains(&plugin.as_str()))
            .collect();
        if held.len() != plugins.len()
            || !crate::guarded_tx::admits_without_provisioning(recorded, publication)?
        {
            return Ok(None);
        }
        program.push(
            Step::new(
                &PLUGIN_WRITERS,
                vec![
                    Value::texts(held.iter().map(|(plugin, ..)| plugin)),
                    Value::Ints(
                        held.iter()
                            .map(|(_, min, _)| Some(i64::from(*min)))
                            .collect(),
                    ),
                    Value::Ints(held.iter().map(|(.., max)| Some(i64::from(*max))).collect()),
                ],
                Some(held.len() as u64),
            ),
            Refusal::Backstop,
        );
    }
    if hint.is_some() {
        guard_hints(&mut program, planner, facts)?;
    }
    let nodes = commit.graph.nodes();
    if !nodes.is_empty() {
        let ids: Vec<&str> = nodes.iter().map(|node| node.node_id.as_str()).collect();
        program.push(
            Step::none(&OCCUPIED, vec![Value::texts(&ids)]),
            Refusal::Occupied,
        );
        let mut bodies = Vec::with_capacity(nodes.len());
        let mut sizes = Vec::with_capacity(nodes.len());
        for node in nodes {
            let body = node.encode_storage_body(fleet).map_err(|error| {
                StoreError::Backend(format!("failed to encode graph node body: {error}"))
            })?;
            sizes.push(Some(i64::try_from(body.len()).map_err(|_| {
                StoreError::Backend("graph node body exceeds PostgreSQL BIGINT".to_owned())
            })?));
            bodies.push(Some(body));
        }
        let planned = plan.planned_node_facts();
        program.push(
            Step::new(
                &NODES,
                vec![
                    text(session),
                    Value::texts(&ids),
                    Value::Texts(
                        nodes
                            .iter()
                            .map(|node| node.parent_node_id.as_ref().map(ToString::to_string))
                            .collect(),
                    ),
                    Value::Ints(
                        planned
                            .iter()
                            .map(|facts| sql_int("node generation", facts.generation).map(Some))
                            .collect::<Result<_, _>>()?,
                    ),
                    Value::texts(planned.iter().map(|facts| facts.frame_node_id.as_str())),
                    Value::Ints(sizes),
                    Value::Texts(bodies),
                ],
                Some(nodes.len() as u64),
            ),
            Refusal::Collision,
        );
    }
    // The published head is a retained revision from this commit on.
    if facts.config_ref.as_deref() != Some(config_ref.as_str()) {
        program.checkpoint.nodes.extend(config_staging.into_nodes());
    }
    program.push(
        Step::one(
            &REVISION,
            vec![
                text(session),
                Value::int(next),
                Value::Text(meta.leaf_node_id.as_ref().map(ToString::to_string)),
                Value::key(program.checkpoint.checkpoint_ref.as_str()),
                Value::key(config_ref.as_str()),
            ],
        ),
        Refusal::Backstop,
    );
    program.push(
        Step::one(
            &HEAD_CAS,
            vec![
                text(session),
                Value::int(next),
                Value::int(sql_int("head revision", plan.actual_head_revision())?),
            ],
        ),
        Refusal::HeadMoved,
    );
    end_frames_left(&mut program, commit, read)?;
    let mut touch = Step::any(&TOUCH, vec![text(session), Value::Now]).capturing(RETENTION);
    if plan.head_changed()
        && let Some(old_leaf) = plan.old_leaf_node_id()
    {
        touch = touch.retiring_ancestry([old_leaf.to_owned()]);
    }
    program.push(touch, Refusal::Backstop);
    settle_ingress(&mut program, commit, read)?;
    acquire_attachments(&mut program, commit)?;
    // `until_gc` releases nothing here. The other policies release what
    // this publication moved out of their window; a session with no
    // metadata row recorded no policy and releases nothing either.
    program.push(
        Step::any(&PRUNE, vec![Value::int(0), text(session)])
            .when_flag(RETENTION, "releases", true),
        Refusal::Backstop,
    );
    for step in super::retention::release_material_steps(session) {
        program.push(
            step.when_flag(RETENTION, "releases", true),
            Refusal::Backstop,
        );
    }
    program.push(
        Step::any(&WORK_REMAINING, vec![text(session)]).capturing(WORK),
        Refusal::Backstop,
    );
    // The receipt is staged on the turn feed, its sequence assigned after
    // the commit (FIG-5276): one for each answer of the work read.
    for work_remaining in [false, true] {
        let mut result = program.result.clone();
        result.work_remaining = work_remaining;
        program.push(
            Step::one(&RECORD_RECEIPT, receipt_values(&plan, &result, next)?).when_flag(
                WORK,
                "work_remaining",
                work_remaining,
            ),
            Refusal::Backstop,
        );
    }
    Ok(Some(program))
}

/// The guards of the hints a one-call commit was planned on: the session's
/// write lock, then each fact the lock decides (no deletion, an admitted
/// session, no receipt of the operation, the head at its revision), and
/// the published leaf, row-locked.
fn guard_hints(
    program: &mut Program,
    planner: &RuntimeCommitPlanner,
    facts: &HeadFacts,
) -> Result<(), StoreError> {
    let session = planner.commit().session_id.as_str();
    // The session's write lock is the authority every history commit and
    // deletion shares; the committing actor's row is locked by its fence.
    let actor = ActorKey::session(session)
        .map_err(|error| StoreError::Backend(format!("session actor key: {error}")))?;
    program.push(
        Step::any(&LOCK_SESSION, vec![text(actor.as_str()), text(session)]),
        Refusal::Backstop,
    );
    program.push(Step::none(&DELETED, vec![text(session)]), Refusal::Deleted);
    // Only a session the catalog admitted commits.
    program.push(
        Step::one(&MATERIALIZED, vec![text(session)]),
        Refusal::NotFound,
    );
    program.push(
        Step::none(&RECEIPT, vec![text(session), text(planner.operation_key())]),
        Refusal::Backstop,
    );
    // The head the plan publishes over, locked, and the leaf it extends.
    program.push(
        if !facts.exists {
            Step::none(&HEAD_ABSENT, vec![text(session)])
        } else {
            Step::one(
                &HEAD_AT,
                vec![
                    text(session),
                    Value::int(sql_int("head revision", facts.revision)?),
                ],
            )
        },
        Refusal::HeadMoved,
    );
    if let PublishedLeafFacts::Live(parent) = &facts.leaf {
        program.push(
            Step::one(&LEAF, vec![text(parent.node_id.as_str())]),
            Refusal::LeafRetired(parent.node_id.clone()),
        );
    }
    Ok(())
}

/// The writes of the frames the commit leaves (ADR 0113 §3.1), decided
/// under their referrer locks: a transition's carries into its successor,
/// each left frame's fence and its `Ended` cleanup, and the SQL share of an
/// ungated end.
fn end_frames_left(
    program: &mut Program,
    commit: &lash_core_store::store::RuntimeCommit,
    read: &ReadFacts,
) -> Result<(), StoreError> {
    if let Some(transition) = &commit.frame_transition {
        let successor = lash_core_store::artifact_referrer::ArtifactReferrer::FrameEnvironment(
            transition.successor.clone(),
        );
        for (namespace, artifact_ref) in &read.carries {
            program.push(
                Step::any(
                    &CARRY_EDGE,
                    vec![
                        text(*namespace),
                        text(artifact_ref.as_str()),
                        text(successor.kind().as_str()),
                        text(successor.canonical_id()),
                    ],
                ),
                Refusal::Backstop,
            );
        }
    }
    let gate = commit
        .frame_transition
        .as_ref()
        .map(|transition| transition.gate.clone());
    for ended in &read.ended {
        let kind = ended.referrer.kind().as_str();
        let id = ended.referrer.canonical_id();
        program.push(
            Step::any(
                &FENCE_REFERRER,
                vec![text(kind), text(id.clone()), Value::Now],
            ),
            Refusal::Backstop,
        );
        let cleanup = lash_core_store::artifact_referrer::ArtifactCleanup::ended(
            ended.referrer.clone(),
            Vec::new(),
            gate.clone(),
        );
        let json = cleanup
            .to_json()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let obligation = lash_core_store::store::ObligationKey::ArtifactCleanup {
            referrer: ended.referrer.clone(),
        }
        .id();
        use lash_core_store::store::CleanupUpsert;
        match ended.decision {
            CleanupUpsert::Insert => program.push(
                Step::any(
                    &CLEANUP_INSERT,
                    vec![
                        text(kind),
                        text(id.clone()),
                        text(json),
                        text(obligation.as_str()),
                        Value::Now,
                    ],
                ),
                Refusal::Backstop,
            ),
            CleanupUpsert::ReplaceGuard => program.push(
                Step::any(
                    &CLEANUP_REPLACE,
                    vec![text(kind), text(id.clone()), text(json), Value::Now],
                ),
                Refusal::Backstop,
            ),
            CleanupUpsert::Keep => {}
        }
        if gate.is_none()
            && ended.decision != CleanupUpsert::Keep
            && ended.referrer.kind().holds_artifacts()
        {
            program.push(
                super::turns::end_local_step(&ended.referrer, None)?,
                Refusal::Backstop,
            );
        }
    }
    Ok(())
}

/// The settlement of every row the commit names under the run that
/// admitted it (FIG-3927), each write predicated on the row's standing:
/// the commands it applied, the inputs and batches it delivered, and the
/// rows a cancel released or dropped.
fn settle_ingress(
    program: &mut Program,
    commit: &lash_core_store::store::RuntimeCommit,
    read: &ReadFacts,
) -> Result<(), StoreError> {
    use lash_core_store::store::IngressRowId;
    use lash_core_store::store::IngressTerminalCause;
    let session = commit.session_id.as_str();
    if let Some(commands) = commit.applied_commands.as_ref() {
        let operation_key = commit.turn_commit.operation.storage_key()?;
        for batch in &commands.batch_ids {
            let cause = match commit.command_outcomes.get(batch) {
                Some(lash_core_store::queued_work_vocabulary::SessionCommandOutcome::ConfigTransaction {
                    outcome: lash_core_store::config_transaction::ConfigTransactionOutcome::Stale { .. },
                }) => IngressTerminalCause::StaleConfigRevision,
                _ => IngressTerminalCause::Applied,
            };
            program.push(
                Step::one(
                    &SETTLE_COMMAND,
                    vec![
                        text(session),
                        text(batch.as_str()),
                        Value::Now,
                        text(cause.as_str()),
                        text(operation_key.clone()),
                    ],
                ),
                Refusal::Command(batch.clone()),
            );
        }
    }
    let Some(ingress) = commit.ingress.as_ref() else {
        return Ok(());
    };
    let run = ingress.run.as_str();
    let settle_input = |input: &lash_vocabulary::identity::InputId, state: &str| {
        Step::one(
            &SETTLE_INPUT,
            vec![
                text(session),
                text(input.as_str()),
                text(state),
                text(run),
                Value::Now,
            ],
        )
    };
    let settle_batch = |batch: &lash_vocabulary::identity::BatchId, cause: IngressTerminalCause| {
        Step::one(
            &SETTLE_BATCH,
            vec![
                text(session),
                text(batch.as_str()),
                text(run),
                text(cause.as_str()),
                Value::Now,
            ],
        )
    };
    let completed = lash_core_store::turn_input_vocabulary::TurnInputStateKind::Completed.as_str();
    for completion in &ingress.completed_inputs {
        for input in &completion.input_ids {
            program.push(
                settle_input(input, completed),
                Refusal::Ingress {
                    run: ingress.run.clone(),
                    row: IngressRowId::Input(input.clone()),
                },
            );
        }
    }
    for completion in &ingress.completed_batches {
        for batch in &completion.batch_ids {
            program.push(
                settle_batch(batch, IngressTerminalCause::Delivered),
                Refusal::Ingress {
                    run: ingress.run.clone(),
                    row: IngressRowId::Batch(batch.clone()),
                },
            );
        }
    }
    use lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy as Disposition;
    for released in &read.released {
        let step = match (&released.row, released.disposition) {
            (IngressRowId::Input(input), Disposition::Defer) => Step::one(
                &RELEASE_INPUT,
                vec![text(session), text(input.as_str()), text(run)],
            ),
            (IngressRowId::Input(input), Disposition::Drop) => settle_input(
                input,
                lash_core_store::turn_input_vocabulary::TurnInputStateKind::Cancelled.as_str(),
            ),
            (IngressRowId::Batch(batch), Disposition::Defer) => Step::one(
                &RELEASE_BATCH,
                vec![text(session), text(batch.as_str()), text(run)],
            ),
            (IngressRowId::Batch(batch), Disposition::Drop) => {
                settle_batch(batch, IngressTerminalCause::Cancelled)
            }
        };
        let refusal = Refusal::Ingress {
            run: ingress.run.clone(),
            row: released.row.clone(),
        };
        program.push(step, refusal);
    }
    Ok(())
}

/// The session's claim of every attachment the commit names: refused once
/// the session's referrer ended, and per attachment, under its fence lock,
/// unless it is being deleted or was never uploaded.
fn acquire_attachments(
    program: &mut Program,
    commit: &lash_core_store::store::RuntimeCommit,
) -> Result<(), StoreError> {
    if commit.committed_attachment_ids.is_empty() {
        return Ok(());
    }
    let referrer =
        lash_core_store::artifact_referrer::ArtifactReferrer::Session(commit.session_id.clone());
    program.push(
        Step::none(
            &REFERRER_FENCED,
            vec![
                text(referrer.kind().as_str()),
                text(referrer.canonical_id()),
            ],
        ),
        Refusal::SessionEnded,
    );
    let ids: BTreeSet<_> = commit.committed_attachment_ids.iter().collect();
    for id in &ids {
        program.push(
            Step::any(
                &LOCK_ATTACHMENT,
                vec![
                    Value::int(i64::from(
                        crate::attachments::ATTACHMENT_FENCE_LOCK_NAMESPACE,
                    )),
                    text(id.as_str()),
                ],
            ),
            Refusal::Backstop,
        );
    }
    for id in &ids {
        program.push(
            Step::none(&ATTACHMENT_DELETING, vec![Value::key(id.as_str())]),
            Refusal::Attachment((*id).clone()),
        );
        program.push(
            Step::one(&ATTACHMENT_UPLOADED, vec![Value::key(id.as_str())]),
            Refusal::Attachment((*id).clone()),
        );
    }
    for id in &ids {
        program.push(
            Step::any(&ATTACHMENT_CLAIMED, vec![Value::key(id.as_str())]),
            Refusal::Backstop,
        );
        program.push(
            Step::any(
                &ATTACHMENT_EDGE,
                vec![
                    Value::key(id.as_str()),
                    text(referrer.kind().as_str()),
                    text(referrer.canonical_id()),
                ],
            ),
            Refusal::Backstop,
        );
        program.push(
            Step::any(
                &super::retention::HOLD_ATTACHMENT,
                vec![
                    text(commit.session_id.as_str()),
                    Value::key(id.as_str()),
                    text(lash_store_sql::attachment::session_holds::HISTORY),
                    text(""),
                ],
            ),
            Refusal::Backstop,
        );
    }
    Ok(())
}

/// The facts the explicit form reads in `tx`, under the session's write
/// lock, which it takes first: the head, its config root admitted as the
/// identity the commit compares (`configs` lends a root a committed read
/// validated), and its leaf, row-locked; the ownership of a store writer's
/// head, an ancestor's reachability, the commit's taken nodes, the rows a
/// cancel released and the cleanups of the frames the commit leaves, each
/// under the lock the commit keeps. `Err(replay)` when the commit already
/// committed.
pub(super) async fn read_facts(
    tx: &mut crate::guarded_tx::GuardedTx<'_>,
    planner: &RuntimeCommitPlanner,
    writer: HeadWriter,
    chunk_size: usize,
    configs: &lash_core_store::store_backend_support::ConfigCache,
) -> Result<Result<Box<Held>, RuntimeCommitReceipt>, StoreError> {
    let commit = planner.commit();
    let fleet = tx.fleet();
    tx.admit_plugin_writers(planner.plugin_publication())
        .await?;
    crate::runtime_persistence::ensure_session_not_deleted_tx(&mut *tx, &commit.session_id).await?;
    let admitted = crate::observed_sql::query_scalar::<_, bool>(
        session_sql().meta_postgres.exists_materialized.sql(),
    )
    .bind(commit.session_id.as_str())
    .fetch_one(crate::observed_sql::executor(&mut ***tx))
    .await
    .map_err(crate::support::store_sqlx_error)?;
    if !admitted {
        return Err(StoreError::SessionNotFound {
            session_id: commit.session_id.clone(),
        });
    }
    let existing = held_head(tx, &commit.session_id, fleet, configs).await?;
    planner.validate_node_derivation()?;
    if let Some(prior) =
        crate::runtime_persistence::committed_receipt_tx(&mut *tx, planner, fleet).await?
        && let Some(replay) = planner.decide_receipt(Some(prior))?
    {
        return Ok(Err(replay.into_result()));
    }
    if writer == HeadWriter::Store {
        let now = crate::support::postgres_transaction_epoch_ms(tx).await?;
        super::bump_unclaimed_session(tx, &commit.session_id, now).await?;
    }
    // The bound turn owns the head (FIG-4202): a write outside every run
    // is refused while a run or an open command owns it. The session lock
    // serializes the read with every admission; the plan's own refusals (a
    // moved head) answer before the ownership's.
    let ownership = if lash_core_store::store::head_write_needs_ownership(
        commit,
        writer,
        existing.as_ref().is_some_and(|head| !head.created),
    ) {
        Some(crate::session_runs::head_ownership_facts_conn(&mut *tx, &commit.session_id).await?)
    } else {
        None
    };
    // Publication owns the complete sorted blob-row set before this fresh
    // commit locks or writes any checkpoint owner edge, graph row, or head:
    // a component the store does not hold refuses the commit here.
    let checkpoint =
        crate::support::put_checkpoint_tx(&mut *tx, &commit.checkpoint, fleet, chunk_size).await?;
    let mut leaf_owner = None;
    let leaf = match existing.as_ref().and_then(|head| head.leaf.clone()) {
        None => PublishedLeafFacts::Absent,
        Some(node_id) => {
            let parent = crate::observed_sql::query_as::<_, (i64, String, String)>(
                session_sql()
                    .graph_postgres
                    .select_parent_facts_for_update
                    .sql(),
            )
            .bind(node_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(crate::support::store_sqlx_error)?;
            match parent {
                Some((generation, frame, owner)) => {
                    leaf_owner = Some(owner);
                    PublishedLeafFacts::Live(ParentNodeFacts {
                        node_id,
                        generation: crate::support::u64_from_sql(
                            "SessionGraph node",
                            "generation",
                            generation,
                        )?,
                        frame_node_id: NodeId::parse(&frame)?,
                    })
                }
                None => PublishedLeafFacts::Retired { node_id },
            }
        }
    };
    let head = HeadFacts {
        exists: existing.is_some(),
        revision: existing.as_ref().map_or(0, |head| head.revision),
        config_ref: existing.as_ref().map(|head| head.config_ref.clone()),
        frame: existing.and_then(|head| head.frame),
        leaf,
    };
    let requested_ancestor_is_active = crate::runtime_persistence::requested_ancestor_is_active_tx(
        &mut *tx, commit, &head.leaf, leaf_owner,
    )
    .await?;
    let ids: Vec<&str> = commit
        .graph
        .nodes()
        .iter()
        .map(|node| node.node_id.as_str())
        .collect();
    let occupied: std::collections::HashSet<NodeId> = if ids.is_empty() {
        Default::default()
    } else {
        crate::observed_sql::query_scalar::<_, String>(
            session_sql().graph_postgres.select_occupied.sql(),
        )
        .bind(&ids)
        .fetch_all(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(crate::support::store_sqlx_error)?
        .into_iter()
        .map(NodeId::parse)
        .collect::<Result<_, _>>()?
    };
    // The frames the commit leaves are the plan's: its derived frame
    // against the head's.
    let plan = planner.plan(lash_core_store::store::FreshRuntimeCommitFacts {
        actual_head_revision: head.revision,
        published_leaf: head.leaf.clone(),
        requested_ancestor_is_active,
        occupied_node_ids: std::collections::HashSet::clone(&occupied),
    })?;
    if let Some(facts) = ownership {
        lash_core_store::store::require_unowned_head(&commit.session_id, facts)?;
    }
    let derived = plan
        .head_meta(lash_core_store::store::BlobRef(String::new()))
        .current_frame_node_id;
    let left = lash_core_store::store::frames_left_by_commit(
        head.frame.as_ref(),
        &commit.graph,
        derived.as_ref(),
    );
    let released = crate::runtime_persistence::released_rows_tx(&mut *tx, commit).await?;
    let (ended, carries) =
        crate::runtime_persistence::frames_left_facts_tx(&mut *tx, commit, &left).await?;
    Ok(Ok(Box::new(Held {
        head,
        read: ReadFacts {
            requested_ancestor_is_active,
            occupied,
            released: released
                .into_iter()
                .map(|(row, disposition, affected)| Released {
                    row,
                    disposition,
                    affected,
                })
                .collect(),
            ended: ended
                .into_iter()
                .map(|(referrer, decision)| EndedFrame { referrer, decision })
                .collect(),
            carries,
        },
        checkpoint,
    })))
}

/// The head row of session `session_id` the explicit form plans over.
struct StoredHead {
    config_ref: String,
    revision: u64,
    leaf: Option<NodeId>,
    frame: Option<FrameNodeId>,
    /// Whether the head is the one a session's admission created, which
    /// no commit published yet.
    created: bool,
}

/// Session `session_id`'s head in `tx`, as a head load reads it under
/// `fleet`, its config root admitted as the commit's validated config
/// identity ([`crate::support::validate_config_root_tx`]).
async fn held_head(
    tx: &mut crate::guarded_tx::GuardedTx<'_>,
    session_id: &lash_vocabulary::identity::SessionId,
    fleet: lash_core_store::store::FleetFormat,
    configs: &lash_core_store::store_backend_support::ConfigCache,
) -> Result<Option<StoredHead>, StoreError> {
    let Some(row) = crate::observed_sql::query(SQL.head.sql())
        .bind(session_id.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(crate::support::store_sqlx_error)?
    else {
        return Ok(None);
    };
    let get = crate::support::store_sqlx_error;
    let revision = crate::support::u64_from_sql("SessionHeadMeta", "head_revision", row.get(1))?;
    let config_ref: Option<String> = crate::keys::get(&row, 0).map_err(get)?;
    let root_held: Option<bool> = row.try_get(5).map_err(get)?;
    let config_ref = crate::support::validate_config_root_tx(
        tx,
        session_id,
        config_ref.as_deref(),
        root_held == Some(true),
        fleet,
        configs,
    )
    .await?;
    let frame: Option<String> = row.try_get(4).map_err(get)?;
    let leaf: Option<String> = row.try_get(2).map_err(get)?;
    if let Some(leaf) = &leaf
        && frame.is_none()
    {
        return Err(StoreError::MissingFrameOpenAncestor {
            leaf_node_id: NodeId::parse(leaf.clone())?,
        });
    }
    let frame =
        frame
            .map(FrameNodeId::new)
            .transpose()
            .map_err(|error| StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: error.to_string(),
            })?;
    let checkpoint: Option<String> = crate::keys::get(&row, 3).map_err(get)?;
    let leaf = leaf.map(NodeId::parse).transpose()?;
    let config_ref = config_ref.into_named()?;
    Ok(Some(StoredHead {
        config_ref,
        revision,
        created: revision == 0 && leaf.is_none() && checkpoint.is_none(),
        leaf,
        frame,
    }))
}

/// Name the refusal of `program`'s step `index`, reading in `tx`.
pub(super) async fn classify(
    tx: &mut sqlx::PgConnection,
    commit: &lash_core_store::store::RuntimeCommit,
    program: &Program,
    refused: &super::steps::Refused,
) -> StoreError {
    let session = &commit.session_id;
    let backstop = || {
        StoreError::Backend(format!(
            "head commit step `{}` of session `{session}` answered other rows than its plan",
            program.steps[refused.index].entry.name()
        ))
    };
    match &program.refusals[refused.index] {
        Refusal::Backstop => backstop(),
        Refusal::Deleted => StoreError::SessionDeleted {
            session_id: session.clone(),
        },
        Refusal::NotFound => StoreError::SessionNotFound {
            session_id: session.clone(),
        },
        Refusal::HeadMoved => {
            let actual = crate::observed_sql::query_scalar::<_, i64>(
                session_sql().head.select_revision.sql(),
            )
            .bind(session.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await;
            match actual {
                Ok(actual) => StoreError::HeadRevisionConflict {
                    expected: commit.expected_head_revision,
                    actual: actual.map_or(0, |actual| u64::try_from(actual).unwrap_or(0)),
                },
                Err(error) => crate::support::store_sqlx_error(error),
            }
        }
        Refusal::LeafRetired(node_id) => StoreError::InvalidGraphLeaf {
            leaf_node_id: Some(node_id.clone()),
        },
        Refusal::Occupied => {
            let taken: BTreeSet<String> = refused
                .rows
                .iter()
                .filter_map(|row| row.try_get::<String, _>(0).ok())
                .collect();
            match commit
                .graph
                .nodes()
                .iter()
                .find(|node| taken.contains(node.node_id.as_str()))
            {
                Some(node) => StoreError::NodeIdCollision {
                    node_id: node.node_id.clone(),
                },
                None => backstop(),
            }
        }
        Refusal::Collision => {
            let inserted: BTreeSet<String> = refused
                .rows
                .iter()
                .filter_map(|row| row.try_get::<String, _>(0).ok())
                .collect();
            let planned = program.steps[refused.index].values.get(3);
            let Some((index, node)) = commit
                .graph
                .nodes()
                .iter()
                .enumerate()
                .find(|(_, node)| !inserted.contains(node.node_id.as_str()))
            else {
                return backstop();
            };
            let generation = match planned {
                Some(Value::Ints(generations)) => generations
                    .get(index)
                    .copied()
                    .flatten()
                    .and_then(|generation| u64::try_from(generation).ok())
                    .unwrap_or(0),
                _ => 0,
            };
            crate::support::graph_node_collision(tx, session, generation, &node.node_id).await
        }
        Refusal::Command(batch) => {
            crate::runtime_persistence::command_refusal_tx(tx, session, batch).await
        }
        Refusal::Ingress { run, row } => {
            crate::runtime_persistence::ingress_refusal_tx(tx, session, run, row).await
        }
        Refusal::SessionEnded => StoreError::ArtifactReferrerEnded {
            referrer: lash_core_store::artifact_referrer::ArtifactReferrer::Session(
                session.clone(),
            ),
        },
        Refusal::Attachment(id) => StoreError::UnknownAttachment { digest: id.clone() },
    }
}

/// Apply the commit `planner` prepared in the explicit form's transaction
/// `tx` at `now`, by `writer`: its facts read under the session's lock,
/// its checkpoint put among them, then its program run in `tx`, a refusal
/// named. The
/// receipt it answers, or the one its first commit recorded.
pub(super) async fn apply(
    tx: &mut crate::guarded_tx::GuardedTx<'_>,
    planner: &RuntimeCommitPlanner,
    writer: HeadWriter,
    context: &super::steps::Context<'_>,
    chunk_size: usize,
    configs: &lash_core_store::store_backend_support::ConfigCache,
) -> Result<RuntimeCommitReceipt, StoreError> {
    let commit = planner.commit();
    if writer == HeadWriter::Store
        && super::turns::head_writer(context.actor, &commit.session_id) == HeadWriter::Store
    {
        super::PostgresDurableStore::lock_cross_target(tx, &commit.session_id).await?;
    }
    let held = match read_facts(tx, planner, writer, chunk_size, configs).await? {
        Ok(held) => held,
        Err(replayed) => return Ok(replayed),
    };
    let now = u64::try_from(context.now.0).unwrap_or(0);
    let Some(program) = program(planner, Basis::Held(held), now)? else {
        return Err(StoreError::Backend(
            "the explicit head commit read every fact its plan needs".to_owned(),
        ));
    };
    super::install_staged(tx, &program.checkpoint.nodes)
        .await
        .map_err(crate::support::graph_store_error)?;
    let slots = match super::steps::run_capturing(tx, &program.steps, context)
        .await
        .map_err(super::steps::Failure::into_store)?
    {
        Ok(slots) => slots,
        Err(refused) => return Err(classify(tx, commit, &program, &refused).await),
    };
    tx.staged_turn_change();
    let mut result = program.result;
    result.work_remaining = slots
        .get(&WORK)
        .and_then(Option::as_ref)
        .and_then(|row| row.get("work_remaining").copied())
        .unwrap_or(false);
    Ok(result)
}

/// The store's own head commit (`commit_runtime_state`): the commit
/// `planner` prepared, applied in `tx` at `now` as a store writer, its
/// retired roots held for retirement steps before the caller commits.
pub(crate) async fn apply_store_commit(
    tx: &mut crate::guarded_tx::GuardedTx<'_>,
    planner: &RuntimeCommitPlanner,
    now: u64,
    chunk_size: usize,
    configs: &lash_core_store::store_backend_support::ConfigCache,
) -> Result<RuntimeCommitReceipt, StoreError> {
    let session = &planner.commit().session_id;
    let actor = ActorKey::session(session.as_str())
        .map_err(|error| StoreError::Backend(format!("session actor key: {error}")))?;
    let retired = super::Retired::default();
    let now = DurableInstant(sql_int("commit instant", now)?);
    let context = super::steps::Context {
        retired: &retired,
        actor: &actor,
        now,
    };
    let result = apply(
        tx,
        planner,
        HeadWriter::Store,
        &context,
        chunk_size,
        configs,
    )
    .await?;
    super::pages::hold_retired(tx, retired, now)
        .await
        .map_err(crate::support::store_sqlx_error)?;
    Ok(result)
}
