//! Accepted plugin-state mutations journaled with their callback outcome.
use super::*;
use crate::{RuntimeEffectControllerError, RuntimeEffectKind, RuntimeEffectOutcome};
use std::cell::Cell;
use std::future::Future;

thread_local! {
    static READ_ONLY_CONSTRUCTION: Cell<bool> = const { Cell::new(false) };
}

pub(in crate::plugin) struct ReadOnlyConstruction {
    previous: bool,
    thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl ReadOnlyConstruction {
    pub(in crate::plugin) fn enter() -> Self {
        Self {
            previous: READ_ONLY_CONSTRUCTION.with(|scope| scope.replace(true)),
            thread: std::marker::PhantomData,
        }
    }
}

impl Drop for ReadOnlyConstruction {
    fn drop(&mut self) {
        READ_ONLY_CONSTRUCTION.with(|scope| scope.set(self.previous));
    }
}

/// The state accepted by one recorded effect, in accepted batch order.
///
/// Effect-host implementors retain this with the outcome. Replay applies it
/// to the original owner before returning the callback's recorded result.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginStateEffect {
    pub owner: crate::RuntimeOwner,
    pub address: crate::EffectAddress,
    pub mutations: Vec<PluginStateMutation>,
}

/// One accepted atomic batch. The base digest checks the complete namespace,
/// including its format and generation, before replay installs the postimage.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginStateMutation {
    pub plugin: String,
    pub base: crate::BlobRef,
    pub postimage: PluginNamespaceState,
}

struct Capture {
    state: Arc<Mutex<PluginStateRegistry>>,
    record: PluginStateEffect,
    before: Vec<PluginNamespaceState>,
}

tokio::task_local! {
    static ACCEPTED: Arc<Mutex<Capture>>;
}

pub(super) fn require_scope(
    state: &Arc<Mutex<PluginStateRegistry>>,
    plugin: &str,
) -> Result<(), PluginStateError> {
    let admitted = !READ_ONLY_CONSTRUCTION.with(Cell::get)
        && ACCEPTED
            .try_with(|capture| Arc::ptr_eq(&capture.lock_recover().state, state))
            .unwrap_or(false);
    if admitted {
        Ok(())
    } else {
        Err(PluginStateError::WriteScopeRequired {
            plugin: plugin.into(),
        })
    }
}

pub(super) fn record_accepted(
    state: &Arc<Mutex<PluginStateRegistry>>,
    plugin: &str,
    before: &PluginNamespaceState,
    after: &PluginNamespaceState,
) {
    let _ = ACCEPTED.try_with(|capture| {
        let mut capture = capture.lock_recover();
        if Arc::ptr_eq(&capture.state, state) {
            capture.before.push(before.clone());
            capture.record.mutations.push(PluginStateMutation {
                plugin: plugin.into(),
                base: namespace_ref(before),
                postimage: after.clone(),
            });
        }
    });
}

#[expect(
    clippy::expect_used,
    reason = "a namespace contains only serializable JSON values"
)]
fn namespace_ref(namespace: &PluginNamespaceState) -> crate::BlobRef {
    crate::BlobRef::for_content(&rmp_serde::to_vec_named(namespace).expect("namespace encodes"))
}

pub(crate) async fn record_effect<F>(
    plugins: Arc<crate::PluginSession>,
    kind: RuntimeEffectKind,
    address: crate::EffectAddress,
    body: F,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>
where
    F: Future<Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
{
    let capture = Arc::new(Mutex::new(Capture {
        state: Arc::clone(&plugins.state),
        before: Vec::new(),
        record: PluginStateEffect {
            owner: plugins.owner().clone(),
            address,
            mutations: Vec::new(),
        },
    }));
    let mut attempt = AttemptState {
        capture: Arc::clone(&capture),
        completed: false,
    };
    let result = ACCEPTED.scope(Arc::clone(&capture), body).await;
    if result
        .as_ref()
        .is_err_and(RuntimeEffectControllerError::is_attempt_fault)
    {
        return result;
    }
    attempt.completed = true;
    let record = capture.lock_recover().record.clone();
    if record.mutations.is_empty() {
        return result;
    }
    plugins
        .state
        .lock_recover()
        .applied_effects
        .insert(record.address.clone());
    Ok(RuntimeEffectOutcome::PluginState {
        kind,
        state: Box::new(record),
        result: Box::new(result),
    })
}

// An unrecorded body cannot leave its accepted writes for a later commit.
struct AttemptState {
    capture: Arc<Mutex<Capture>>,
    completed: bool,
}

impl Drop for AttemptState {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let (state, mutations) = {
            let capture = self.capture.lock_recover();
            (
                Arc::clone(&capture.state),
                capture
                    .record
                    .mutations
                    .iter()
                    .cloned()
                    .zip(capture.before.iter().cloned())
                    .collect::<Vec<_>>(),
            )
        };
        let mut state = state.lock_recover();
        for (mutation, before) in mutations.into_iter().rev() {
            if state.data.plugins.get(&mutation.plugin) == Some(&mutation.postimage) {
                state.data.plugins.insert(mutation.plugin, before);
                state.source = None;
            }
        }
    }
}

impl crate::PluginSession {
    /// Restore state accepted by a completed callback before serving its result.
    /// The whole candidate validates before any namespace changes.
    pub fn restore_effect_state(
        &self,
        outcome: RuntimeEffectOutcome,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectOutcome::PluginState { state, result, .. } = outcome else {
            return Ok(outcome);
        };
        if state.owner != self.owner {
            return Err(PluginStateError::EffectOwnerMismatch.into());
        }
        let mut live = self.state.lock_recover();
        if live.applied_effects.contains(&state.address) {
            return *result;
        }
        let mut candidate = live.data.clone();
        // A repeated delivery, including the live execution's return, already
        // carries each namespace's last accepted postimage.
        let final_images: BTreeMap<_, _> = state
            .mutations
            .iter()
            .map(|mutation| (mutation.plugin.as_str(), &mutation.postimage))
            .collect();
        let installed: std::collections::BTreeSet<_> = final_images
            .iter()
            .filter(|(id, postimage)| candidate.plugins.get(**id) == Some(*postimage))
            .map(|(id, _)| *id)
            .collect();
        for mutation in &state.mutations {
            if installed.contains(mutation.plugin.as_str()) {
                continue;
            }
            let namespace = candidate.plugins.get(&mutation.plugin).ok_or_else(|| {
                PluginStateError::EffectReplayMismatch {
                    plugin: mutation.plugin.clone(),
                }
            })?;
            if namespace_ref(namespace) != mutation.base {
                return Err(PluginStateError::EffectReplayMismatch {
                    plugin: mutation.plugin.clone(),
                }
                .into());
            }
            validate_namespace(&mutation.postimage.values)?;
            candidate
                .plugins
                .insert(mutation.plugin.clone(), mutation.postimage.clone());
        }
        if candidate != live.data {
            for (id, namespace) in &candidate.plugins {
                let generation = live.acceptance_generations.entry(id.clone()).or_default();
                *generation = (*generation).max(namespace.generation);
            }
            live.data = candidate;
            live.source = None;
        }
        live.applied_effects.insert(state.address);
        *result
    }
}

#[cfg(test)]
pub(super) fn test_scope<T>(store: &PluginStateStore, body: impl FnOnce() -> T) -> T {
    let capture = Arc::new(Mutex::new(Capture {
        state: Arc::clone(&store.state),
        before: Vec::new(),
        record: PluginStateEffect {
            owner: store.owner().clone(),
            address: crate::EffectAddress::new(
                crate::ExecutionScope::turn("fixture", "run"),
                "unit-callback",
            )
            .expect("valid unit callback address"),
            mutations: Vec::new(),
        },
    }));
    ACCEPTED.sync_scope(capture, body)
}
