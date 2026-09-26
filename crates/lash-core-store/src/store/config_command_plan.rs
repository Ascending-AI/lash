//! The one config-command planner (FIG-3541, ADR 0101 §12).
//!
//! A command drain hands the session's running head config and the drain's
//! `ApplyConfigPatch` commands to [`plan_config_commands`]. The plan answers
//! three things: the head config to commit (`running` plus every applied
//! patch, its `config_revision` advanced once per applied patch and never
//! otherwise), the per-command [`IngressCommandResult`] the settlement
//! records, and the refused windows the settlement turns back.
//!
//! Route validation is a caller-supplied callback: the planner owns the
//! decision order — stale first, then validate, then apply — and never the
//! provider table.

use super::{
    ConfigRefusalCode, IngressCommandOutcome, IngressCommandResult, IngressItemId,
    IngressRefusedWindow,
};
use crate::{ApplyConfigPatch, ModelSpec, PersistedSessionConfig};

/// What one drain's [`plan_config_commands`] decided.
#[derive(Clone, Debug)]
pub struct ConfigCommandPlan {
    /// The head config to commit with the drain's command settlement:
    /// `running` plus every applied patch, its `config_revision` advanced
    /// once per applied patch and never otherwise — in particular it does
    /// not move for a stale or refused command.
    pub config: PersistedSessionConfig,
    /// One outcome per drained command, in enqueue order.
    pub outcomes: Vec<IngressCommandOutcome>,
    /// One window per refused command, in the refused commands' enqueue
    /// order; the settlement turns back the open turn-lane rows each window
    /// covers.
    pub refused_windows: Vec<IngressRefusedWindow>,
}

/// Plan one drain's config commands against the session's running head
/// config.
///
/// `commands` is the drain's `ApplyConfigPatch` commands —
/// `(item_id, enqueue_seq, patch)` — processed in enqueue order regardless
/// of the slice's order. Per command:
///
/// 1. `base_config_revision != running.config_revision` settles
///    `StaleConfigRevision`; the patch is neither validated nor applied.
/// 2. A patch that would change `provider_id` or `model` validates the
///    resulting route through `validate`; a refusal settles
///    `Refused { code }`, leaves the running config and its revision alone,
///    and opens a refused window running to the drain's next config command.
/// 3. Otherwise the patch applies and `config_revision` advances by exactly
///    one, even when the overlay restates current values.
///
/// Every command settles — applied, stale, or refused — and the drain
/// commits `plan.config` as its head either way.
pub fn plan_config_commands(
    running: &PersistedSessionConfig,
    commands: &[(IngressItemId, u64, &ApplyConfigPatch)],
    validate: &dyn Fn(&str, &ModelSpec) -> Result<(), ConfigRefusalCode>,
) -> ConfigCommandPlan {
    let mut ordered: Vec<&(IngressItemId, u64, &ApplyConfigPatch)> = commands.iter().collect();
    ordered.sort_by_key(|entry| entry.1);

    let mut config = running.clone();
    let mut outcomes = Vec::with_capacity(ordered.len());
    let mut refused_windows = Vec::new();
    for (index, entry) in ordered.iter().enumerate() {
        let (item_id, seq, patch) = (&entry.0, entry.1, entry.2);
        let outcome = |result| IngressCommandOutcome {
            item_id: item_id.clone(),
            result,
        };
        if patch.base_config_revision != config.config_revision {
            outcomes.push(outcome(IngressCommandResult::StaleConfigRevision {
                base: patch.base_config_revision,
                head: config.config_revision,
            }));
            continue;
        }
        if patch_changes_route(patch, &config) {
            let provider_id = patch
                .provider_id
                .as_deref()
                .map_or(config.provider_id.as_str(), str::trim)
                .to_string();
            let model = patch.model.clone().unwrap_or_else(|| config.model.clone());
            if let Err(code) = validate(&provider_id, &model) {
                outcomes.push(outcome(IngressCommandResult::Refused { code }));
                refused_windows.push(IngressRefusedWindow {
                    after: seq,
                    before: ordered.get(index + 1).map(|next| next.1),
                    code,
                });
                continue;
            }
        }
        patch.apply_to_persisted_config(&mut config);
        config.config_revision = config.config_revision.saturating_add(1);
        outcomes.push(outcome(IngressCommandResult::Applied));
    }
    ConfigCommandPlan {
        config,
        outcomes,
        refused_windows,
    }
}

/// Whether the patch would move the head to a different provider/model
/// route: it names a `provider_id` whose recorded (trimmed) form differs
/// from the running one, or a `model` that differs. Naming the current
/// route restates it — nothing to validate, the patch just applies.
fn patch_changes_route(patch: &ApplyConfigPatch, config: &PersistedSessionConfig) -> bool {
    patch
        .provider_id
        .as_deref()
        .is_some_and(|id| id.trim() != config.provider_id)
        || patch
            .model
            .as_ref()
            .is_some_and(|model| *model != config.model)
}

#[cfg(test)]
#[path = "config_command_plan/tests.rs"]
mod tests;
