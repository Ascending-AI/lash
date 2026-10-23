//! Session-owned source collection and fenced environment publication.
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lash_core_store::environment::*;
use lash_durable::environment::{EnvironmentStore, EnvironmentWrite};
use lash_durable::{ActorTx, CommitLabel, DomainWrite, DurableError, Epoch};
use lash_protocol_types::sync::MutexExt;

use crate::plugin::environment_sources::{EnvironmentSources, SourceCollectionError};
use crate::{ActorContext, Session};

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentAdmissionError {
    #[error(transparent)]
    Source(#[from] SourceCollectionError),
    #[error(transparent)]
    Assembly(#[from] AssemblyRefusal),
    #[error(transparent)]
    Durable(#[from] DurableError),
    #[error(transparent)]
    Plugin(#[from] lash_execution_contracts::PluginError),
}

impl EnvironmentAdmissionError {
    pub(super) fn into_runtime_error(self) -> crate::RuntimeError {
        match self.into_turn_error() {
            crate::runtime::durable::session::TurnError::Runtime(error) => error,
            crate::runtime::durable::session::TurnError::Durable(error) => {
                lash_execution_contracts::plugin_error::registry_error(error)
                    .into_turn_failure(crate::RuntimeErrorCode::SessionHeadRefresh)
            }
            error => crate::RuntimeError::new(
                crate::RuntimeErrorCode::SessionHeadRefresh,
                error.to_string(),
            ),
        }
    }
    pub(super) fn into_turn_error(self) -> crate::runtime::durable::session::TurnError {
        use crate::runtime::durable::session::TurnError;
        match self {
            Self::Durable(DurableError::Environment(error)) => {
                TurnError::Runtime(structured_refusal(
                    "lash.environment_owner",
                    &error,
                    crate::RuntimeErrorCode::StoreCommitFailed,
                ))
            }
            Self::Durable(error) => TurnError::Durable(error),
            Self::Plugin(error) => TurnError::Runtime(
                error.into_turn_failure(crate::RuntimeErrorCode::ToolCatalogResolutionFailed),
            ),
            Self::Source(SourceCollectionError::Provider(error)) => TurnError::Runtime(
                error.into_turn_failure(crate::RuntimeErrorCode::ToolCatalogResolutionFailed),
            ),
            Self::Source(error) => TurnError::Runtime(structured_refusal(
                "lash.environment_source",
                &error,
                crate::RuntimeErrorCode::SessionHeadRefresh,
            )),
            Self::Assembly(error) => TurnError::Runtime(structured_refusal(
                "lash.environment_assembly",
                &error,
                crate::RuntimeErrorCode::ToolCatalogResolutionFailed,
            )),
        }
    }
}
fn structured_refusal<T: serde::Serialize + std::fmt::Display>(
    kind: &str,
    error: &T,
    code: crate::RuntimeErrorCode,
) -> crate::RuntimeError {
    let payload = match serde_json::to_value(error) {
        Ok(payload) => payload,
        Err(error) => {
            return crate::RuntimeError::new(
                crate::RuntimeErrorCode::RecordEncodingFailed,
                error.to_string(),
            );
        }
    };
    let failure = lash_protocol_types::PluginOperationFailure {
        error_type: kind.into(),
        error_version: std::num::NonZeroU32::MIN,
        payload,
        class: lash_protocol_types::PluginFailureClass::Redrivable,
        code: crate::FailureCode::from(&code),
        message: error.to_string(),
        origin: None,
    };
    crate::RuntimeError::new(code, error.to_string()).with_cause(
        crate::RuntimeErrorCause::PluginOperation {
            failure: Box::new(failure),
        },
    )
}

impl From<lash_durable::environment::EnvironmentRefusal> for EnvironmentAdmissionError {
    fn from(error: lash_durable::environment::EnvironmentRefusal) -> Self {
        Self::Durable(error.into())
    }
}

/// Notifications only append facts. A boundary acknowledges its exact prefix
/// after the transaction carrying its head and round has committed.
pub(super) struct SessionEnvironment {
    sources: EnvironmentSources,
    #[cfg(test)]
    pub assemblies: std::sync::atomic::AtomicUsize,
    mailbox: Arc<Mutex<Vec<(uuid::Uuid, EnvironmentChange)>>>,
    state: Mutex<Resident>,
    registered: BTreeSet<SourceId>,
}
#[derive(Default)]
struct Resident {
    epoch: Option<Epoch>,
    reconcile: bool,
    head: Option<EnvironmentHead>,
    body: Option<Arc<EnvironmentBody>>,
    handle: Option<crate::session::ToolCatalogHandle>,
}

#[derive(Clone)]
pub(super) struct Adoption {
    pub round: RoundEnvironment,
    pub body: Arc<EnvironmentBody>,
    pub handle: crate::session::ToolCatalogHandle,
    pub publication: Option<Arc<Publication>>,
    pub observed: BTreeSet<uuid::Uuid>,
    pub frontier: InputFrontier,
}

pub(super) struct Publication {
    resident: Arc<SessionEnvironment>,
    expected: Option<EnvironmentHead>,
    next: Option<(
        EnvironmentHead,
        Arc<EnvironmentBody>,
        crate::session::ToolCatalogHandle,
    )>,
    owners: Vec<VersionOwnerId>,
    changes: BTreeSet<uuid::Uuid>,
    acknowledged: AtomicBool,
}
impl std::fmt::Debug for Publication {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Publication")
            .field("owners", &self.owners)
            .finish_non_exhaustive()
    }
}
impl Publication {
    pub fn encode_head(
        &self,
        staging: &mut lash_core_store::checkpoint_graph::GraphStaging,
    ) -> Result<(), DurableError> {
        if !self.acknowledged.load(Ordering::Acquire)
            && let Some((head, _, _)) = &self.next
        {
            lash_durable::environment::stage_head(staging, head)?;
        }
        Ok(())
    }
    pub fn stage(&self, tx: &mut ActorTx) {
        if self.acknowledged.load(Ordering::Acquire) {
            return;
        }
        for owner in &self.owners {
            tx.write(DomainWrite::Environment(EnvironmentWrite::Publish {
                owner: owner.clone(),
            }));
        }
        if let Some((head, _, _)) = &self.next {
            tx.write(DomainWrite::Environment(EnvironmentWrite::Head {
                expected: self.expected.clone(),
                head: Some(head.clone()),
            }));
        }
    }
    pub fn committed(&self) {
        if self.acknowledged.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut state = self.resident.state.lock_recover();
        if let Some((head, body, handle)) = &self.next {
            state.head = Some(head.clone());
            state.body = Some(body.clone());
            state.handle = Some(handle.clone());
        }
        self.resident
            .mailbox
            .lock_recover()
            .retain(|(id, _)| !self.changes.contains(id));
    }
}

impl SessionEnvironment {
    pub async fn materialize(
        session: &Session,
        store: &dyn EnvironmentStore,
    ) -> Result<Arc<Self>, EnvironmentAdmissionError> {
        let mailbox = Arc::new(Mutex::new(Vec::new()));
        let target = mailbox.clone();
        let sources = session
            .plugins()
            .environment_sources(Arc::new(move |change| {
                target.lock_recover().push((uuid::Uuid::new_v4(), change));
            }))?;
        let head = store.environment_head(session.session_id()).await?;
        let body = match &head {
            Some(head) => Some(load_body(store, &head.round()).await?),
            None => None,
        };
        let handle = None;
        let registered = sources
            .descriptors()
            .map(|descriptor| descriptor.source)
            .collect();
        Ok(Arc::new(Self {
            sources,
            registered,
            #[cfg(test)]
            assemblies: Default::default(),
            mailbox,
            state: Mutex::new(Resident {
                head,
                body,
                handle,
                ..Default::default()
            }),
        }))
    }

    fn assemble(
        &self,
        previous: Option<&EnvironmentBody>,
        delta: Vec<SourceSnapshot>,
        config: &crate::OwnerConfigView,
        frontier: &mut InputFrontier,
    ) -> Result<Arc<EnvironmentBody>, EnvironmentAdmissionError> {
        #[cfg(test)]
        self.assemblies.fetch_add(1, Ordering::SeqCst);
        for snapshot in &delta {
            frontier.insert(snapshot.source.clone(), snapshot.revision.clone());
        }
        let body = assemble(previous, delta)?;
        let projections = self.sources.project(
            body.fragments()
                .iter()
                .map(|(id, fragment)| (id, fragment.fragment())),
            config,
        )?;
        for snapshot in &projections {
            frontier.insert(snapshot.source.clone(), snapshot.revision.clone());
        }
        Ok(Arc::new(if projections.is_empty() {
            body
        } else {
            assemble(Some(&body), projections)?
        }))
    }

    /// A restored phase can follow a committed publication whose acknowledgement
    /// was lost. Only the next live boundary reloads the head; restore itself
    /// uses the recorded owner, including when that owner is an older version.
    pub fn invalidate_adoption(&self) {
        self.state.lock_recover().reconcile = true;
    }

    pub fn refresh(&self) {
        self.mailbox.lock_recover().push((
            uuid::Uuid::new_v4(),
            EnvironmentChange::Refresh { sources: None },
        ));
    }

    /// The session actor serializes boundaries. Provider work runs outside the
    /// resident lock; publication reclassifies under its epoch and whole-head CAS.
    /// Callbacks can only append to the independent mailbox.
    pub async fn adopt(
        self: &Arc<Self>,
        cx: &ActorContext,
        session: &Session,
        run: &crate::ResolvedRun,
        current: Option<&Adoption>,
    ) -> Result<Adoption, EnvironmentAdmissionError> {
        let store = cx.backend().durable().environments();
        let reconcile = {
            let state = self.state.lock_recover();
            state.reconcile || state.epoch != Some(cx.epoch())
        };
        let reload = self.state.lock_recover().epoch.is_some();
        if reconcile && reload {
            let head = store.environment_head(session.session_id()).await?;
            let body = match &head {
                Some(head) => Some(load_body(store.as_ref(), &head.round()).await?),
                None => None,
            };
            let handle = None;
            let mut state = self.state.lock_recover();
            *state = Resident {
                head,
                body,
                handle,
                epoch: Some(cx.epoch()),
                reconcile: false,
            };
        }
        if reconcile {
            {
                let mut state = self.state.lock_recover();
                state.epoch = Some(cx.epoch());
                state.reconcile = false;
            }
            self.mailbox.lock_recover().push((
                uuid::Uuid::new_v4(),
                EnvironmentChange::ConfigSelection {
                    sources: self.registered.clone(),
                },
            ));
        }
        let (previous, body, handle) = {
            let state = self.state.lock_recover();
            (state.head.clone(), state.body.clone(), state.handle.clone())
        };
        let registered = self.registered.clone();
        let changes = self.mailbox.lock_recover().clone();
        let notices: BTreeSet<_> = changes.iter().map(|(id, _)| *id).collect();
        if run.has_override() {
            if let Some(current) = current
                && current.observed == notices
                && !reconcile
            {
                return Ok(current.clone());
            }
            let config =
                crate::plugin::OwnerConfigView::session(session.plugins().admitted_plugin_config());
            let mut frontier = current
                .filter(|_| !reconcile)
                .map(|current| current.frontier.clone())
                .or_else(|| previous.as_ref().map(|head| head.input_frontier.clone()))
                .unwrap_or_default();
            let initial =
                (current.is_none() || reconcile).then(|| EnvironmentChange::ConfigSelection {
                    sources: registered.clone(),
                });
            let changes = changes.into_iter()
                .filter(|(id, _)| reconcile || current.is_none_or(|current| !current.observed.contains(id)))
                .map(|(_, change)| change)
                .filter(|change| !matches!(change, EnvironmentChange::SourceRevision { source, .. } if !registered.contains(source)))
                .chain(initial);
            let mut delta = self.sources.collect(&frontier, changes, &config)?;
            let prior = current
                .map(|current| current.body.as_ref())
                .or(body.as_deref());
            delta.extend(detached(prior, &registered, &frontier));
            if delta.is_empty()
                && let Some(current) = current
            {
                let mut adopted = current.clone();
                adopted.observed = notices;
                return Ok(adopted);
            }
            let body = self.assemble(prior, delta, &config, &mut frontier)?;
            frontier.retain(|source, _| registered.contains(source));
            if let Some(current) = current
                && current.body.id() == body.id()
            {
                let mut adopted = current.clone();
                adopted.observed = notices;
                adopted.frontier = frontier;
                return Ok(adopted);
            }
            let handle = session.project_environment(&body)?;
            let owner = prepare(cx, session, &body, &handle).await?;
            let round = RoundEnvironment {
                owner: owner.clone(),
                version: body.id().clone(),
            };
            let publication = Some(Arc::new(Publication {
                resident: self.clone(),
                expected: previous,
                next: None,
                owners: vec![owner],
                changes: BTreeSet::new(),
                acknowledged: AtomicBool::new(false),
            }));
            return Ok(Adoption {
                round,
                body,
                handle,
                publication,
                observed: notices,
                frontier,
            });
        }
        // Detached producers cannot make stale revision notices authoritative.
        let changes = changes.into_iter().map(|(_, change)| change).filter(|change| !matches!(change, EnvironmentChange::SourceRevision { source, .. } if !registered.contains(source)));
        let frontier = previous
            .as_ref()
            .map(|h| h.input_frontier.clone())
            .unwrap_or_default();
        let config =
            crate::plugin::OwnerConfigView::session(session.plugins().admitted_plugin_config());
        let mut delta = self.sources.collect(&frontier, changes, &config)?;
        delta.extend(detached(body.as_deref(), &registered, &frontier));
        let mut owners = Vec::new();
        let mut next = None;
        let (base_head, base_body, base_handle) = if !delta.is_empty() || body.is_none() {
            let mut frontier = frontier;
            let candidate = self.assemble(body.as_deref(), delta, &config, &mut frontier)?;
            frontier.retain(|source, _| registered.contains(source));
            let (body, handle, owner) = if body
                .as_ref()
                .is_some_and(|body| body.id() == candidate.id())
            {
                let owner = previous
                    .as_ref()
                    .ok_or(lash_durable::environment::EnvironmentRefusal::Content)?
                    .owner
                    .clone();
                let handle = match handle {
                    Some(handle) => handle,
                    None => session.project_environment(&candidate)?,
                };
                (candidate, handle, owner)
            } else {
                let handle = session.project_environment(&candidate)?;
                let owner = prepare(cx, session, &candidate, &handle).await?;
                owners.push(owner.clone());
                (candidate, handle, owner)
            };
            let head = EnvironmentHead {
                version: body.id().clone(),
                owner: owner.clone(),
                input_frontier: frontier,
            };
            next = Some((head.clone(), body.clone(), handle.clone()));
            (head, body, handle)
        } else {
            (
                previous
                    .clone()
                    .ok_or(lash_durable::environment::EnvironmentRefusal::Content)?,
                body.ok_or(lash_durable::environment::EnvironmentRefusal::Content)?,
                handle.ok_or(lash_durable::environment::EnvironmentRefusal::Content)?,
            )
        };
        let frontier = base_head.input_frontier.clone();
        let (round, body, handle) = (base_head.round(), base_body, base_handle);
        let publication = (next.is_some() || !notices.is_empty()).then(|| {
            Arc::new(Publication {
                resident: self.clone(),
                expected: previous,
                next,
                owners,
                changes: notices,
                acknowledged: AtomicBool::new(false),
            })
        });
        Ok(Adoption {
            round,
            body,
            handle,
            publication,
            observed: BTreeSet::new(),
            frontier,
        })
    }
}

async fn prepare(
    cx: &ActorContext,
    session: &Session,
    body: &EnvironmentBody,
    handle: &crate::session::ToolCatalogHandle,
) -> Result<VersionOwnerId, EnvironmentAdmissionError> {
    let owner = VersionOwner::preparing(body.id().clone());
    let mut tx = cx.begin().await?;
    tx.write(DomainWrite::Environment(EnvironmentWrite::prepare(
        owner.clone(),
        body,
    )?));
    cx.commit(tx, CommitLabel::new("environment.prepare"))
        .await?;
    session
        .plugins()
        .composition()
        .retain_environment_packages(
            owner.id(),
            handle.surface().packages(),
            cx.backend().stores().package_store().as_ref(),
        )
        .await
        .map_err(lash_execution_contracts::PluginError::from)?;
    Ok(owner.id().clone())
}

pub(super) async fn load_body(
    store: &dyn EnvironmentStore,
    round: &RoundEnvironment,
) -> Result<Arc<EnvironmentBody>, DurableError> {
    let record = store
        .environment_owner(&round.owner)
        .await?
        .ok_or_else(|| {
            lash_durable::environment::EnvironmentRefusal::Missing(round.owner.clone())
        })?;
    if record.owner.state() != VersionOwnerState::Published {
        return Err(lash_durable::environment::EnvironmentRefusal::NotPublished(
            round.owner.clone(),
        )
        .into());
    }
    let body = record
        .body
        .ok_or(lash_durable::environment::EnvironmentRefusal::Content)?;
    if body.id() != &round.version || record.owner.version() != &round.version {
        return Err(lash_durable::environment::EnvironmentRefusal::Content.into());
    }
    Ok(Arc::new(body))
}

/// Detachment names the old source, including a run-only source absent from the
/// base frontier. Absence is an observed fact, with its own control revision.
fn detached(
    previous: Option<&EnvironmentBody>,
    registered: &BTreeSet<SourceId>,
    frontier: &InputFrontier,
) -> Vec<SourceSnapshot> {
    previous
        .into_iter()
        .flat_map(|body| body.fragments().keys())
        .filter(|source| !registered.contains(*source))
        .map(|source| SourceSnapshot {
            source: source.clone(),
            revision: frontier
                .get(source)
                .cloned()
                .unwrap_or_else(|| SourceRevision {
                    incarnation: uuid::Uuid::new_v4(),
                    counter: 0,
                }),
            fragment: EnvironmentFragment::default(),
        })
        .collect()
}
