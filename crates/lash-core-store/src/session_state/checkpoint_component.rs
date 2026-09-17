//! Resident checkpoint component shapes: ADR 0056's commit states as types.
//!
//! `ResidentCheckpointComponent` is either `Unchanged` — the next commit
//! reuses its durable descriptor — or `Changed` — the next commit writes a
//! body whose payload is present by construction. The released-body shell of
//! an unchanged component is `ResidentCheckpointComponentBody`'s payload
//! `None`.

#[derive(Clone, Debug, serde::Serialize)]
pub(super) enum ResidentCheckpointComponentBody {
    ToolState {
        snapshot: Option<crate::ToolState>,
        generation: Option<u64>,
    },
    PluginState {
        snapshot: Option<crate::PluginState>,
        generations: std::collections::BTreeMap<String, u64>,
    },
    ExecutionState(Option<Vec<u8>>),
    Opaque(Option<Vec<u8>>),
}

/// A body pending its next commit: ADR 0056's "present body" state, where the
/// payload is always present.
#[derive(Clone, Debug, serde::Serialize)]
pub(super) enum PendingCheckpointComponentBody {
    ToolState {
        snapshot: crate::ToolState,
        generation: Option<u64>,
    },
    PluginState {
        snapshot: crate::PluginState,
        generations: std::collections::BTreeMap<String, u64>,
    },
    ExecutionState(Vec<u8>),
    Opaque(Vec<u8>),
}

impl PendingCheckpointComponentBody {
    /// The same body kept resident after its commit: the durable ref becomes
    /// authoritative and the payload stays a second copy until released.
    pub(super) fn into_resident(self) -> ResidentCheckpointComponentBody {
        match self {
            Self::ToolState {
                snapshot,
                generation,
            } => ResidentCheckpointComponentBody::ToolState {
                snapshot: Some(snapshot),
                generation,
            },
            Self::PluginState {
                snapshot,
                generations,
            } => ResidentCheckpointComponentBody::PluginState {
                snapshot: Some(snapshot),
                generations,
            },
            Self::ExecutionState(bytes) => {
                ResidentCheckpointComponentBody::ExecutionState(Some(bytes))
            }
            Self::Opaque(bytes) => ResidentCheckpointComponentBody::Opaque(Some(bytes)),
        }
    }
}

/// A resident checkpoint component in one of ADR 0056's commit states.
///
/// `Unchanged` always carries the durable ref the next commit reuses, and
/// `Changed` always carries the body it writes — the `(descriptor, body,
/// dirty)` combinations that used to surface as `StoredDataCorrupt` are not
/// representable.
#[derive(Clone, Debug, serde::Serialize)]
pub(super) enum ResidentCheckpointComponent {
    /// The next commit reuses `descriptor`; `body` is the still-resident copy
    /// or a released shell (payload `None`).
    Unchanged {
        descriptor: crate::CheckpointComponentDescriptor,
        body: ResidentCheckpointComponentBody,
    },
    /// The next commit writes `body`; `descriptor` is the durable ref it
    /// supersedes when this is a local edit over a committed component.
    Changed {
        descriptor: Option<crate::CheckpointComponentDescriptor>,
        body: PendingCheckpointComponentBody,
    },
}

impl ResidentCheckpointComponent {
    pub(super) fn descriptor(&self) -> Option<&crate::CheckpointComponentDescriptor> {
        match self {
            Self::Unchanged { descriptor, .. } => Some(descriptor),
            Self::Changed { descriptor, .. } => descriptor.as_ref(),
        }
    }

    pub(super) fn tool_state_snapshot(&self) -> Option<&crate::ToolState> {
        match self {
            Self::Unchanged {
                body: ResidentCheckpointComponentBody::ToolState { snapshot, .. },
                ..
            } => snapshot.as_ref(),
            Self::Changed {
                body: PendingCheckpointComponentBody::ToolState { snapshot, .. },
                ..
            } => Some(snapshot),
            _ => None,
        }
    }

    pub(super) fn tool_state_generation(&self) -> Option<u64> {
        match self {
            Self::Unchanged {
                body: ResidentCheckpointComponentBody::ToolState { generation, .. },
                ..
            } => *generation,
            Self::Changed {
                body: PendingCheckpointComponentBody::ToolState { generation, .. },
                ..
            } => *generation,
            _ => None,
        }
    }

    pub(super) fn plugin_state_snapshot(&self) -> Option<&crate::PluginState> {
        match self {
            Self::Unchanged {
                body: ResidentCheckpointComponentBody::PluginState { snapshot, .. },
                ..
            } => snapshot.as_ref(),
            Self::Changed {
                body: PendingCheckpointComponentBody::PluginState { snapshot, .. },
                ..
            } => Some(snapshot),
            _ => None,
        }
    }

    pub(super) fn plugin_generations(&self) -> Option<&std::collections::BTreeMap<String, u64>> {
        match self {
            Self::Unchanged {
                body: ResidentCheckpointComponentBody::PluginState { generations, .. },
                ..
            }
            | Self::Changed {
                body: PendingCheckpointComponentBody::PluginState { generations, .. },
                ..
            } => Some(generations),
            _ => None,
        }
    }

    pub(super) fn execution_state_body(&self) -> Option<&[u8]> {
        match self {
            Self::Unchanged {
                body: ResidentCheckpointComponentBody::ExecutionState(body),
                ..
            } => body.as_deref(),
            Self::Changed {
                body: PendingCheckpointComponentBody::ExecutionState(body),
                ..
            } => Some(body),
            _ => None,
        }
    }

    /// The resident bytes of a keyed execution-state leaf.
    pub(super) fn opaque_body(&self) -> Option<&[u8]> {
        match self {
            Self::Unchanged {
                body: ResidentCheckpointComponentBody::Opaque(body),
                ..
            } => body.as_deref(),
            Self::Changed {
                body: PendingCheckpointComponentBody::Opaque(body),
                ..
            } => Some(body),
            _ => None,
        }
    }

    /// Adopts the committed manifest's durable ref. A changed body stays
    /// resident as the unchanged component's second copy.
    pub(super) fn adopt_descriptor(&mut self, descriptor: crate::CheckpointComponentDescriptor) {
        let body = match self {
            Self::Unchanged { body, .. } => {
                std::mem::replace(body, ResidentCheckpointComponentBody::Opaque(None))
            }
            Self::Changed { body, .. } => {
                std::mem::replace(body, PendingCheckpointComponentBody::Opaque(Vec::new()))
                    .into_resident()
            }
        };
        *self = Self::Unchanged { descriptor, body };
    }

    /// Releases the tool and plugin snapshot payloads. A changed typed body
    /// was the pending write's only copy; with the write discarded the
    /// component demotes to the durable ref it superseded, and reports whether
    /// anything durable remains to track. Accepted execution bodies are the
    /// storeless restore's source and stay.
    pub(super) fn release_typed_snapshot(&mut self) -> bool {
        let released = match self {
            Self::Unchanged { body, .. } => {
                match body {
                    ResidentCheckpointComponentBody::ToolState { snapshot, .. } => *snapshot = None,
                    ResidentCheckpointComponentBody::PluginState { snapshot, .. } => {
                        *snapshot = None
                    }
                    ResidentCheckpointComponentBody::ExecutionState(_)
                    | ResidentCheckpointComponentBody::Opaque(_) => {}
                }
                return true;
            }
            Self::Changed { descriptor, body } => {
                let released = match body {
                    PendingCheckpointComponentBody::ToolState { generation, .. } => {
                        ResidentCheckpointComponentBody::ToolState {
                            snapshot: None,
                            generation: *generation,
                        }
                    }
                    PendingCheckpointComponentBody::PluginState { generations, .. } => {
                        ResidentCheckpointComponentBody::PluginState {
                            snapshot: None,
                            generations: std::mem::take(generations),
                        }
                    }
                    PendingCheckpointComponentBody::ExecutionState(_)
                    | PendingCheckpointComponentBody::Opaque(_) => return true,
                };
                (descriptor, released)
            }
        };
        match released.0.take() {
            Some(descriptor) => {
                *self = Self::Unchanged {
                    descriptor,
                    body: released.1,
                };
                true
            }
            None => false,
        }
    }

    /// Releases every resident payload of an unchanged component: once the
    /// durable ref is authoritative, the encoded bytes are a second resident
    /// copy. A changed body has not committed yet; a discarded pending write
    /// demotes to the durable ref it superseded, except a changed opaque leaf,
    /// which is the retry's only source and stays.
    pub(super) fn release_body(&mut self) -> bool {
        let released = match self {
            Self::Unchanged { body, .. } => {
                match body {
                    ResidentCheckpointComponentBody::ToolState { snapshot, .. } => *snapshot = None,
                    ResidentCheckpointComponentBody::PluginState { snapshot, .. } => {
                        *snapshot = None
                    }
                    ResidentCheckpointComponentBody::ExecutionState(payload)
                    | ResidentCheckpointComponentBody::Opaque(payload) => *payload = None,
                }
                return true;
            }
            Self::Changed { descriptor, body } => {
                let released = match body {
                    PendingCheckpointComponentBody::ToolState { generation, .. } => {
                        ResidentCheckpointComponentBody::ToolState {
                            snapshot: None,
                            generation: *generation,
                        }
                    }
                    PendingCheckpointComponentBody::PluginState { generations, .. } => {
                        ResidentCheckpointComponentBody::PluginState {
                            snapshot: None,
                            generations: std::mem::take(generations),
                        }
                    }
                    PendingCheckpointComponentBody::ExecutionState(_) => {
                        ResidentCheckpointComponentBody::ExecutionState(None)
                    }
                    PendingCheckpointComponentBody::Opaque(_) => return true,
                };
                (descriptor, released)
            }
        };
        match released.0.take() {
            Some(descriptor) => {
                *self = Self::Unchanged {
                    descriptor,
                    body: released.1,
                };
                true
            }
            None => false,
        }
    }
}
