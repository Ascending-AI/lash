use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TriggerMutationOutcome {
    Created,
    Unchanged,
    Updated,
    Enabled,
    Disabled,
    Deleted,
    Revived,
}

/// The recorded mutation result. Every subscription fact belongs to `record`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerMutationReceipt {
    pub disposition: TriggerMutationOutcome,
    pub record: TriggerSubscriptionRecord,
}

/// A script-visible trigger, sharing the public registration body with list.
/// Internal execution captures and stored row metadata never enter this view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "trigger_handle")]
pub struct TriggerHandle {
    pub id: String,
    /// A list observes state without performing a mutation and answers `None`.
    pub disposition: Option<TriggerMutationOutcome>,
    #[serde(flatten)]
    pub registration: TriggerRegistration,
}

impl From<&TriggerSubscriptionRecord> for TriggerHandle {
    fn from(record: &TriggerSubscriptionRecord) -> Self {
        Self {
            id: record.subscription_key.clone(),
            disposition: None,
            registration: TriggerRegistration::from(record),
        }
    }
}

/// Projects a mutation into the same public trigger body a list observes.
pub fn trigger_handle(receipt: &TriggerMutationReceipt) -> TriggerHandle {
    TriggerHandle {
        disposition: Some(receipt.disposition),
        ..TriggerHandle::from(&receipt.record)
    }
}

/// Encodes the typed handle for the registration intent's realized result.
pub fn trigger_handle_outcome_value(
    receipt: &TriggerMutationReceipt,
) -> Result<serde_json::Value, PluginError> {
    serde_json::to_value(trigger_handle(receipt))
        .map_err(|err| PluginError::Session(format!("failed to encode trigger handle: {err}")))
}

impl TriggerMutationReceipt {
    pub(super) fn from_record(
        record: TriggerSubscriptionRecord,
        disposition: TriggerMutationOutcome,
    ) -> Self {
        Self {
            disposition,
            record,
        }
    }

    pub fn owner_scope(&self) -> &TriggerOwnerScope {
        &self.record.owner_scope
    }
    pub fn subscription_key(&self) -> &str {
        &self.record.subscription_key
    }
    pub fn subscription_id(&self) -> &str {
        &self.record.subscription_id
    }
    pub fn incarnation(&self) -> &str {
        &self.record.incarnation
    }
    pub fn revision(&self) -> u64 {
        self.record.revision
    }
    pub fn definition_fingerprint(&self) -> &str {
        &self.record.definition_fingerprint
    }
    pub fn enabled(&self) -> bool {
        self.record.lifecycle.enabled()
    }
}

/// The complete verb vocabulary for reading and changing durable trigger
/// subscription state.
///
/// A command paired with [`TriggerStore::execute_command`] is **the** supported
/// route for a host to mutate trigger subscriptions. The durable tables behind
/// a store (`lash_*` in the first-party SQL backends) are private to lash:
/// their columns and the record JSON they hold are stable only within one
/// schema version, and a hand-written `UPDATE` also bypasses the revision fence
/// and the operation receipt the store writes in the same transaction. Trigger
/// state corrupted that way has no supported repair.
///
/// Three properties make the surface safe to retry:
///
/// - **Fenced.** Every point mutation carries `expected_revision`, taken from a
///   [`List`](Self::List) record or from an earlier receipt. A writer that lost
///   the race receives [`TriggerOperationError::Conflict`] instead of silently
///   overwriting the winner.
/// - **Receipted.** `operation_id` journals whatever the store evaluated, a
///   committed mutation or a conflict, so replaying one operation returns its
///   original [`TriggerMutationReceipt`] instead of re-evaluating against newer
///   state.
/// - **Keyed.** `subscription_key` is unique within a [`TriggerOwnerScope`], so
///   a command names its target directly and never needs a store-assigned
///   lookup handle.
///
/// [`Enable`](Self::Enable) is a first-class verb, so re-enabling is fully
/// supported: read the live revision, then `Enable` against it. Registering the
/// same definition again is deliberately *not* a re-enable: it reports
/// [`TriggerMutationOutcome::Unchanged`] and leaves the row disabled.
///
/// ```no_run
/// use lash_core::{
///     ProcessOriginator, TriggerCommand, TriggerCommandOutcome, TriggerOperationError,
///     TriggerOwnerScope, TriggerStore, TriggerSubscriptionFilter,
/// };
///
/// /// Re-enable one subscription without touching a `lash_*` table.
/// /// Returns `false` when the owner scope holds no live row for the key.
/// async fn reenable(
///     store: &dyn TriggerStore,
///     owner_scope: TriggerOwnerScope,
///     actor: ProcessOriginator,
///     subscription_key: &str,
/// ) -> Result<bool, TriggerOperationError> {
///     // Read the live revision through the same command surface. `List` is
///     // owner-scoped, excludes tombstones, and is never receipted.
///     let TriggerCommandOutcome::List { records } = store
///         .execute_command(
///             "reenable-read",
///             TriggerCommand::List {
///                 owner_scope: owner_scope.clone(),
///                 filter: TriggerSubscriptionFilter {
///                     subscription_key: Some(subscription_key.to_string()),
///                     ..TriggerSubscriptionFilter::default()
///                 },
///             },
///         )
///         .await??
///     else {
///         unreachable!("List returns list records")
///     };
///     let Some(record) = records.into_iter().next() else {
///         return Ok(false);
///     };
///
///     // Fence the write on that revision. A concurrent writer that moved the
///     // row first turns this into a conflict; re-read and retry, never patch
///     // the row by hand. The operation id makes the retry idempotent.
///     let TriggerCommandOutcome::Mutation { receipt } = store
///         .execute_command(
///             &format!("reenable:{subscription_key}:{}", record.revision),
///             TriggerCommand::Enable {
///                 owner_scope,
///                 actor,
///                 subscription_key: subscription_key.to_string(),
///                 expected_revision: record.revision,
///             },
///         )
///         .await??
///     else {
///         unreachable!("Enable returns one mutation receipt")
///     };
///     assert!(receipt.enabled());
///     assert!(receipt.revision() > record.revision);
///     Ok(true)
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TriggerCommand {
    /// An identical definition is idempotent; a changed one conflicts instead of upserting.
    Register {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        draft: TriggerSubscriptionDraft,
    },
    /// This is the supported lookup by key, name, source, or enablement, and the read that
    /// supplies `expected_revision` to every mutation below.
    List {
        owner_scope: TriggerOwnerScope,
        filter: TriggerSubscriptionFilter,
    },
    /// Replace the definition of a live subscription at `expected_revision`.
    Update {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_key: String,
        draft: TriggerSubscriptionDraft,
        expected_revision: u64,
    },
    /// Resume occurrence delivery for a disabled subscription at
    /// `expected_revision`. This is the re-enable verb.
    Enable {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_key: String,
        expected_revision: u64,
    },
    /// Stop matching new occurrences at `expected_revision`, keeping the
    /// definition and every already-reserved delivery.
    Disable {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_key: String,
        expected_revision: u64,
    },
    /// Tombstone a live subscription at `expected_revision`, preserving the
    /// delivery history that references its incarnation.
    Delete {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_key: String,
        expected_revision: u64,
    },
    /// Bring a tombstoned key back under a new incarnation at
    /// `expected_revision`, which plain [`Register`](Self::Register) refuses.
    Revive {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_key: String,
        draft: TriggerSubscriptionDraft,
        expected_revision: u64,
    },
    /// Delete several keys the caller owns in one journaled operation, skipping
    /// keys that are absent or already tombstoned.
    Prune {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_keys: Vec<String>,
    },
}

impl TriggerCommand {
    /// Exposes owner scope to store and durable-substrate implementors while persisting trigger
    /// subscriptions and occurrences.
    pub fn owner_scope(&self) -> &TriggerOwnerScope {
        match self {
            Self::Register { owner_scope, .. }
            | Self::List { owner_scope, .. }
            | Self::Update { owner_scope, .. }
            | Self::Enable { owner_scope, .. }
            | Self::Disable { owner_scope, .. }
            | Self::Delete { owner_scope, .. }
            | Self::Revive { owner_scope, .. }
            | Self::Prune { owner_scope, .. } => owner_scope,
        }
    }

    pub fn subscription_key(&self) -> Option<&str> {
        match self {
            Self::Register { draft, .. } => Some(&draft.subscription_key),
            Self::List { .. } => None,
            Self::Update {
                subscription_key, ..
            }
            | Self::Enable {
                subscription_key, ..
            }
            | Self::Disable {
                subscription_key, ..
            }
            | Self::Delete {
                subscription_key, ..
            }
            | Self::Revive {
                subscription_key, ..
            } => Some(subscription_key),
            Self::Prune { .. } => None,
        }
    }

    /// Lets trigger-store implementors distinguish the read-only list command from every command
    /// that may change durable trigger state.
    pub fn is_mutation(&self) -> bool {
        !matches!(self, Self::List { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerCommandOutcome {
    Mutation {
        receipt: Box<TriggerMutationReceipt>,
    },
    List {
        records: Vec<TriggerSubscriptionRecord>,
    },
    Prune {
        receipts: Vec<TriggerMutationReceipt>,
    },
}

#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error, schemars::JsonSchema,
)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TriggerOperationError {
    #[error(
        "trigger subscription conflict for `{subscription_key}`: {reason}; existing revision {existing_revision:?}, existing definition {existing_definition_fingerprint:?}, requested definition {requested_definition_fingerprint:?}"
    )]
    Conflict {
        subscription_key: String,
        existing_revision: Option<u64>,
        existing_definition_fingerprint: Option<String>,
        requested_definition_fingerprint: Option<String>,
        reason: String,
    },
    #[error("trigger subscription request is invalid: {message}")]
    Invalid { message: String },
    #[error(
        "trigger subscription `{subscription_key}` revision cannot advance past {current_revision}"
    )]
    RevisionOverflow {
        subscription_key: String,
        current_revision: u64,
    },
    #[error("trigger subscription operation failed: {message}")]
    Store { message: String },
}

impl TriggerOperationError {
    /// Whether the same operation is refused until its input or configuration changes.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Conflict { .. } | Self::Invalid { .. } | Self::RevisionOverflow { .. } => true,
            Self::Store { .. } => false,
        }
    }

    /// The Lash-vocabulary code a recorded trigger failure carries in the
    /// durable effect summary. Guarded by `PROCESS_EVENT_VOCABULARY_VERSION`:
    /// a spelling change rewrites what a redrive re-derives.
    pub fn failure_code(&self) -> lash_sansio::FailureCode {
        lash_sansio::FailureCode::lash(lash_sansio::TurnFailureCode::from_wire(self.code()))
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Conflict { .. } => "trigger_conflict",
            Self::Invalid { .. } => "trigger_invalid",
            Self::RevisionOverflow { .. } => "trigger_revision_overflow",
            Self::Store { .. } => "trigger_store",
        }
    }
}

impl From<PluginError> for TriggerOperationError {
    fn from(value: PluginError) -> Self {
        Self::Store {
            message: value.to_string(),
        }
    }
}

pub type TriggerEffectResult = Result<TriggerCommandOutcome, TriggerOperationError>;

pub fn next_trigger_revision(
    record: &TriggerSubscriptionRecord,
) -> Result<u64, TriggerOperationError> {
    if record.revision >= i64::MAX as u64 {
        return Err(TriggerOperationError::RevisionOverflow {
            subscription_key: record.subscription_key.clone(),
            current_revision: record.revision,
        });
    }
    Ok(record.revision + 1)
}

pub fn next_trigger_store_revision(record: &TriggerSubscriptionRecord) -> Result<u64, PluginError> {
    if record.revision >= i64::MAX as u64 {
        return Err(PluginError::MonotonicCounterOverflow {
            counter: "trigger_subscription_revision".to_string(),
            current: record.revision,
        });
    }
    Ok(record.revision + 1)
}

// Measured 112 B on rustc 1.97.0, x86_64-unknown-linux-gnu (FIG-595).
const _: () = assert!(std::mem::size_of::<TriggerEffectResult>() <= 144);

pub fn evaluate_trigger_prune(
    records: impl IntoIterator<Item = TriggerSubscriptionRecord>,
    owner_scope: TriggerOwnerScope,
    actor: crate::ProcessOriginator,
    subscription_keys: Vec<String>,
    now: u64,
) -> TriggerEffectResult {
    let requested = subscription_keys
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut receipts = Vec::new();
    for record in records {
        if record.owner_scope != owner_scope
            || record.is_tombstoned()
            || !requested.contains(&record.subscription_key)
        {
            continue;
        }
        let command = TriggerCommand::Delete {
            owner_scope: owner_scope.clone(),
            actor: actor.clone(),
            subscription_key: record.subscription_key.clone(),
            expected_revision: record.revision,
        };
        // A delete keeps the row's incarnation and writes none.
        let incarnation = record.incarnation.clone();
        match evaluate_trigger_mutation_with_incarnation(Some(record), command, now, incarnation)??
        {
            TriggerCommandOutcome::Mutation { receipt } => receipts.push(*receipt),
            TriggerCommandOutcome::List { .. } | TriggerCommandOutcome::Prune { .. } => {
                unreachable!("delete mutation always returns one receipt")
            }
        }
    }
    receipts.sort_by(|left, right| left.subscription_key().cmp(right.subscription_key()));
    Ok(TriggerCommandOutcome::Prune { receipts })
}

/// version_guard(
///     items(trigger_command_preimage),
///     items(
///         path = "crates/lash-core-execution/src/triggers/router.rs",
///         path = "crates/lash-core-execution/src/runtime/process/identity_projection.rs",
///         project_trigger_owner, project_trigger_actor, project_trigger_draft,
///         project_process_event_type, project_process_value_selector,
///         project_trigger_process_input, project_process_payload_leaf, project_process_schema_leaf,
///     ),
/// )
/// version_surface = "coexist"
pub(super) const TRIGGER_COMMAND_FAMILY_VERSION: u8 = 8;
/// version_guard(
///     items(trigger_operation_receipt_preimage),
///     items(path = "crates/lash-core-execution/src/triggers/router.rs", project_trigger_owner),
/// )
/// version_surface = "coexist"
const TRIGGER_OPERATION_ADDRESS_FAMILY_VERSION: u8 = 2;

/// Fingerprint one trigger command independently of its caller-supplied
/// operation-id lookup address.
///
/// Permanent command tags: 1 register, 2 list, 3 update, 4 enable, 5 disable,
/// 6 delete, 7 revive, 8 prune. Retired tags remain burned. Nested owner,
/// actor, draft, and JSON tags are registered beside the trigger-definition
/// projection they share; nested projections carry no version of their own.
pub(super) fn trigger_command_preimage(command: &TriggerCommand) -> Vec<u8> {
    let family_version = TRIGGER_COMMAND_FAMILY_VERSION;
    let mut fingerprint =
        crate::stable_identity::IdentityEncoder::new("lash.trigger-command", family_version);
    match command {
        TriggerCommand::Register {
            owner_scope,
            actor,
            draft,
        } => {
            fingerprint.tag(1);
            project_trigger_owner(&mut fingerprint, owner_scope);
            project_trigger_actor(&mut fingerprint, actor);
            project_trigger_draft(&mut fingerprint, draft);
        }
        TriggerCommand::List {
            owner_scope,
            filter,
        } => {
            fingerprint.tag(2);
            project_trigger_owner(&mut fingerprint, owner_scope);
            let TriggerSubscriptionFilter {
                registrant_scope_id,
                subscription_key,
                name,
                source_type,
                source_key,
                target,
                enabled,
            } = filter;
            for value in [
                registrant_scope_id.as_deref(),
                None,
                subscription_key.as_deref(),
                name.as_deref(),
                source_type.as_deref(),
                source_key.as_deref(),
            ] {
                fingerprint.optional(value, |fingerprint, value| fingerprint.string(value));
            }
            fingerprint.optional(target.as_ref(), project_process_payload_leaf);
            fingerprint.optional(*enabled, |fingerprint, enabled| {
                fingerprint.tag(u8::from(enabled));
            });
        }
        TriggerCommand::Update {
            owner_scope,
            actor,
            subscription_key,
            draft,
            expected_revision,
        } => {
            fingerprint.tag(3);
            project_trigger_owner(&mut fingerprint, owner_scope);
            project_trigger_actor(&mut fingerprint, actor);
            fingerprint.string(subscription_key);
            project_trigger_draft(&mut fingerprint, draft);
            fingerprint.u64(*expected_revision);
        }
        TriggerCommand::Enable {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        } => {
            fingerprint.tag(4);
            project_trigger_owner(&mut fingerprint, owner_scope);
            project_trigger_actor(&mut fingerprint, actor);
            fingerprint.string(subscription_key);
            fingerprint.u64(*expected_revision);
        }
        TriggerCommand::Disable {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        } => {
            fingerprint.tag(5);
            project_trigger_owner(&mut fingerprint, owner_scope);
            project_trigger_actor(&mut fingerprint, actor);
            fingerprint.string(subscription_key);
            fingerprint.u64(*expected_revision);
        }
        TriggerCommand::Delete {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        } => {
            fingerprint.tag(6);
            project_trigger_owner(&mut fingerprint, owner_scope);
            project_trigger_actor(&mut fingerprint, actor);
            fingerprint.string(subscription_key);
            fingerprint.u64(*expected_revision);
        }
        TriggerCommand::Revive {
            owner_scope,
            actor,
            subscription_key,
            draft,
            expected_revision,
        } => {
            fingerprint.tag(7);
            project_trigger_owner(&mut fingerprint, owner_scope);
            project_trigger_actor(&mut fingerprint, actor);
            fingerprint.string(subscription_key);
            project_trigger_draft(&mut fingerprint, draft);
            fingerprint.u64(*expected_revision);
        }
        TriggerCommand::Prune {
            owner_scope,
            actor,
            subscription_keys,
        } => {
            fingerprint.tag(8);
            project_trigger_owner(&mut fingerprint, owner_scope);
            project_trigger_actor(&mut fingerprint, actor);
            fingerprint.sequence(subscription_keys.iter(), |fingerprint, key| {
                fingerprint.string(key)
            });
        }
    }
    fingerprint.finish()
}

pub fn trigger_command_fingerprint(command: &TriggerCommand) -> String {
    let family_version = TRIGGER_COMMAND_FAMILY_VERSION;
    let preimage = trigger_command_preimage(command);
    crate::stable_identity::rendered_hash("trigger-command", family_version, &preimage)
}

pub fn trigger_operation_receipt_id(owner_scope: &TriggerOwnerScope, operation_id: &str) -> String {
    // The fixed-size v2 caller-operation address is independent from the
    // command fingerprint and safe for indexed store keys of any input size.
    let preimage = trigger_operation_receipt_preimage(owner_scope, operation_id);
    crate::stable_identity::rendered_hash(
        "trigger-operation",
        TRIGGER_OPERATION_ADDRESS_FAMILY_VERSION,
        &preimage,
    )
}

pub(super) fn trigger_operation_receipt_preimage(
    owner_scope: &TriggerOwnerScope,
    operation_id: &str,
) -> Vec<u8> {
    let mut address = crate::stable_identity::IdentityEncoder::new(
        "lash.trigger-operation-address",
        TRIGGER_OPERATION_ADDRESS_FAMILY_VERSION,
    );
    project_trigger_owner(&mut address, owner_scope);
    address.string(operation_id);
    address.finish()
}
