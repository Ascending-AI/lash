//! Session-owned source collection and fenced environment publication.
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lash_core_store::environment::*;
use lash_durable::environment::{EnvironmentStore, EnvironmentWrite};
use lash_durable::{ActorTx, CommitLabel, DomainWrite, DurableError, Epoch};
use lash_protocol_types::sync::MutexExt;

use crate::plugin::environment_sources::SourceCollectionError;
use crate::{ActorContext, Session};
use lash_core_execution::session::environment::{
    ClaimedEnvironment, CollectedRunEnvironment, EnvironmentProjection, ReadyEnvironment,
    ResidentEnvironment as Resident,
};

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
    /// Candidate validation retains its typed refusal before the run starts.
    /// Model-boundary failures use the turn-boundary classification below.
    pub(super) fn into_config_validation_error(self) -> crate::RuntimeError {
        match self {
            Self::Plugin(error) => crate::RuntimeEffectControllerError::from(error).into_runtime_error(),
            Self::Source(SourceCollectionError::Provider(error)) =>
                crate::RuntimeEffectControllerError::from(*error).into_runtime_error(),
            Self::Source(SourceCollectionError::Assembly(error)) =>
                Self::Assembly(*error).into_config_validation_error(),
            Self::Assembly(error) => crate::RuntimeEffectControllerError::run_shape_refused(
                crate::RunShapeRefusal::Owner {
                    refusal: lash_execution_contracts::config_error::CoreConfigRefusal::environment_refusal(error),
                },
            ).into_runtime_error(),
            error => error.into_runtime_error(),
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
            Self::Assembly(error @ (AssemblyRefusal::OperationCollision { .. } | AssemblyRefusal::DuplicateTool { .. })) => TurnError::Runtime(
                crate::RuntimeEffectControllerError::run_shape_refused(
                    crate::RunShapeRefusal::Owner {
                        refusal: lash_execution_contracts::config_error::CoreConfigRefusal::environment_refusal(error),
                    },
                ).into_runtime_error(),
            ),
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
    pub shared: Arc<ClaimedEnvironment>,
}
impl std::ops::Deref for SessionEnvironment {
    type Target = ClaimedEnvironment;
    fn deref(&self) -> &Self::Target {
        &self.shared
    }
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

/// The exact mailbox prefix one model boundary adopts. The barrier decision
/// and adoption share these facts, so later mail cannot make a clean sync write.
#[derive(Clone)]
pub(super) struct AdoptionFrontier {
    reconcile: bool,
    changes: Vec<(uuid::Uuid, EnvironmentChange)>,
}

impl AdoptionFrontier {
    pub fn may_prepare(&self, current: Option<&Adoption>, is_override: bool) -> bool {
        self.reconcile
            || current.is_none_or(|current| {
                self.changes
                    .iter()
                    .any(|(id, _)| !is_override || !current.observed.contains(id))
            })
    }
}

pub(super) struct Publication {
    resident: Arc<SessionEnvironment>,
    expected: Option<EnvironmentHead>,
    next: Option<ReadyEnvironment>,
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
            && let Some(next) = &self.next
            && self.expected.as_ref() != Some(&next.head)
        {
            lash_durable::environment::stage_head(staging, &next.head)?;
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
        // Cold reconciliation may only rebuild the native projection of
        // the same durable head. Acknowledge it locally without adding a
        // write that a warm owner would never produce.
        if let Some(next) = &self.next
            && self.expected.as_ref() != Some(&next.head)
        {
            tx.write(DomainWrite::Environment(EnvironmentWrite::Head {
                expected: self.expected.clone(),
                head: Some(next.head.clone()),
            }));
        }
    }
    pub fn committed(&self) {
        if self.acknowledged.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut state = self.resident.state.lock_recover();
        if let Some(next) = &self.next {
            state.projection = EnvironmentProjection::Ready(next.clone());
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
                target
                    .lock_recover()
                    .push((lash_core_store::environment::fresh_identity(), change));
            }))?;
        let projection = hydrate(store, session.session_id()).await?;
        let registered = sources
            .descriptors()
            .map(|descriptor| descriptor.source)
            .collect();
        Ok(Arc::new(Self {
            shared: Arc::new(ClaimedEnvironment {
                plugins: session.plugins().clone(),
                sources,
                registered,
                #[cfg(any(test, feature = "testing"))]
                assemblies: Default::default(),
                mailbox,
                state: Mutex::new(Resident {
                    projection,
                    ..Default::default()
                }),
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

    /// Validate a warm override using the claim's collected source facts. A
    /// cold claim retains ordinary candidate validation until materialization.
    pub fn validate_run_override(
        &self,
        run: &crate::ResolvedRun,
        epoch: Epoch,
        fleet_format: crate::FleetFormat,
        validate: impl FnOnce() -> Result<
            Arc<crate::PluginSession>,
            lash_execution_contracts::PluginError,
        >,
    ) -> Result<bool, EnvironmentAdmissionError> {
        {
            let state = self.state.lock_recover();
            if state.epoch != Some(epoch) || state.reconcile {
                return Ok(false);
            }
        }
        let changes = self.mailbox.lock_recover().clone();
        let observed = changes.iter().map(|(id, _)| *id).collect::<BTreeSet<_>>();
        let reused = {
            let state = self.state.lock_recover();
            state.run_collection.as_ref().is_some_and(|prior| {
                prior.base.as_ref() == state.projection.head()
                    && prior.config == run.config().plugin_config
                    && prior.observed == observed
            })
        };
        // Preserve refusal order: candidate binders judge their configuration
        // before any source observes it. An equal validated candidate needs neither.
        let preview = (!reused).then(validate).transpose()?;
        let collected = self.collect_run_override(run, &changes, false, fleet_format)?;
        if let Some(preview) = preview {
            preview.project_environment(&collected.body)?;
        }
        self.state.lock_recover().run_collection = Some(collected);
        Ok(true)
    }

    /// Collection never owns package lifetimes. Reuse is confined to this
    /// acknowledged claim and exact base head; notices stay pending for the base.
    fn collect_run_override(
        &self,
        run: &crate::ResolvedRun,
        changes: &[(uuid::Uuid, EnvironmentChange)],
        reconcile: bool,
        fleet_format: crate::FleetFormat,
    ) -> Result<CollectedRunEnvironment, EnvironmentAdmissionError> {
        let (base, prior) = {
            let state = self.state.lock_recover();
            let prior = state
                .run_collection
                .as_ref()
                .filter(|prior| !reconcile && prior.base.as_ref() == state.projection.head())
                .cloned()
                .or_else(|| match &state.projection {
                    EnvironmentProjection::Ready(ready) => Some(CollectedRunEnvironment {
                        base: Some(ready.head.clone()),
                        config: run.base().plugin_config.clone(),
                        body: ready.body.clone(),
                        handle: ready.handle.clone(),
                        frontier: ready.frontier.clone(),
                        observed: BTreeSet::new(),
                    }),
                    EnvironmentProjection::Empty | EnvironmentProjection::Hydrated { .. } => None,
                });
            (state.projection.head().cloned(), prior)
        };
        let config = crate::plugin::OwnerConfigView::session(crate::AdmittedPluginConfig::new(
            run.config().plugin_config.clone(),
            run.config().config_revision,
        ));
        let old_config = prior.as_ref().map(|prior| &prior.config);
        let mut changed_owners = BTreeSet::new();
        for owner in run.config().plugin_config.namespaces().keys().chain(
            old_config
                .into_iter()
                .flat_map(|config| config.namespaces().keys()),
        ) {
            if old_config.and_then(|config| config.namespace(owner))
                != run.config().plugin_config.namespace(owner)
            {
                changed_owners.insert(owner.as_str());
            }
        }
        let selection = if reconcile {
            EnvironmentChange::ConfigSelection {
                sources: self.registered.clone(),
            }
        } else {
            self.sources.config_committed(changed_owners)
        };
        let unseen = changes.iter()
            .filter(|(id, _)| prior.as_ref().is_none_or(|prior| !prior.observed.contains(id)))
            .map(|(_, change)| change.clone())
            .filter(|change| !matches!(change, EnvironmentChange::SourceRevision { source, .. } if !self.registered.contains(source)))
            .chain([selection]);
        let mut frontier = prior
            .as_ref()
            .map(|prior| prior.frontier.clone())
            .unwrap_or_default();
        let mut delta = self.sources.collect(&frontier, unseen, &config)?;
        delta.extend(detached(
            prior.as_ref().map(|prior| prior.body.as_ref()),
            &self.registered,
            &frontier,
        ));
        let (body, handle) = if delta.is_empty()
            && let Some(prior) = &prior
        {
            (prior.body.clone(), prior.handle.clone())
        } else {
            let body = self.assemble(
                prior.as_ref().map(|prior| prior.body.as_ref()),
                delta,
                &config,
                &mut frontier,
            )?;
            let handle = Session::project_environment_for(&self.plugins, fleet_format, &body)?;
            (body, handle)
        };
        frontier.retain(|source, _| self.registered.contains(source));
        Ok(CollectedRunEnvironment {
            base,
            config: run.config().plugin_config.clone(),
            body,
            handle,
            frontier,
            observed: changes.iter().map(|(id, _)| *id).collect(),
        })
    }

    /// A restored phase can follow a committed publication whose acknowledgement
    /// was lost. Only the next live boundary reloads the head; restore itself
    /// uses the recorded owner, including when that owner is an older version.
    pub fn invalidate_adoption(&self) {
        self.state.lock_recover().reconcile = true;
    }

    pub fn refresh(&self) {
        self.mailbox.lock_recover().push((
            lash_core_store::environment::fresh_identity(),
            EnvironmentChange::Refresh { sources: None },
        ));
    }

    pub fn capture_frontier(&self, epoch: Epoch) -> AdoptionFrontier {
        let state = self.state.lock_recover();
        let reconcile = state.reconcile || state.epoch != Some(epoch);
        let mut mailbox = self.mailbox.lock_recover();
        if reconcile {
            mailbox.push((
                lash_core_store::environment::fresh_identity(),
                EnvironmentChange::ConfigSelection {
                    sources: self.registered.clone(),
                },
            ));
        }
        AdoptionFrontier {
            reconcile,
            changes: mailbox.clone(),
        }
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
        frontier: AdoptionFrontier,
    ) -> Result<Adoption, EnvironmentAdmissionError> {
        let store = cx.backend().durable().environments();
        let AdoptionFrontier { reconcile, changes } = frontier;
        let reload = self.state.lock_recover().epoch.is_some();
        if reconcile && reload {
            let projection = hydrate(store.as_ref(), session.session_id()).await?;
            let mut state = self.state.lock_recover();
            *state = Resident {
                projection,
                epoch: Some(cx.epoch()),
                ..Default::default()
            };
        }
        if reconcile {
            let mut state = self.state.lock_recover();
            state.epoch = Some(cx.epoch());
            state.reconcile = false;
        }
        let projection = self.state.lock_recover().projection.clone();
        let previous = projection.head().cloned();
        let registered = self.registered.clone();
        let notices: BTreeSet<_> = changes.iter().map(|(id, _)| *id).collect();
        if run.has_override() {
            if let Some(current) = current
                && current.observed == notices
                && !reconcile
            {
                return Ok(current.clone());
            }
            let collected =
                self.collect_run_override(run, &changes, reconcile, session.fleet_format())?;
            let CollectedRunEnvironment {
                body,
                handle,
                frontier,
                ..
            } = &collected;
            self.state.lock_recover().run_collection = Some(collected.clone());
            if let Some(current) = current
                && current.body.id() == body.id()
            {
                let mut adopted = current.clone();
                adopted.observed = notices;
                adopted.frontier = frontier.clone();
                return Ok(adopted);
            }
            let body = body.clone();
            let handle = handle.clone();
            let frontier = frontier.clone();
            drop(handle.preamble());
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
        let (body, mut frontier) = match &projection {
            EnvironmentProjection::Empty => (None, InputFrontier::default()),
            EnvironmentProjection::Hydrated { head, body } => {
                (Some(body.as_ref()), head.input_frontier.clone())
            }
            EnvironmentProjection::Ready(ready) => {
                (Some(ready.body.as_ref()), ready.frontier.clone())
            }
        };
        let config =
            crate::plugin::OwnerConfigView::session(session.plugins().admitted_plugin_config());
        let mut delta = self.sources.collect(&frontier, changes, &config)?;
        delta.extend(detached(body, &registered, &frontier));
        let mut owners = Vec::new();
        let mut next = None;
        let ready = match &projection {
            EnvironmentProjection::Ready(ready) if delta.is_empty() => ready.clone(),
            projection => {
                let candidate = self.assemble(body, delta, &config, &mut frontier)?;
                frontier.retain(|source, _| registered.contains(source));
                let (head, handle) = match projection {
                    EnvironmentProjection::Hydrated { head, body }
                        if body.id() == candidate.id() =>
                    {
                        (head.clone(), session.project_environment(&candidate)?)
                    }
                    EnvironmentProjection::Ready(ready) if ready.body.id() == candidate.id() => {
                        (ready.head.clone(), ready.handle.clone())
                    }
                    EnvironmentProjection::Empty
                    | EnvironmentProjection::Hydrated { .. }
                    | EnvironmentProjection::Ready(_) => {
                        let handle = session.project_environment(&candidate)?;
                        let owner = prepare(cx, session, &candidate, &handle).await?;
                        owners.push(owner.clone());
                        let head = EnvironmentHead {
                            version: candidate.id().clone(),
                            owner,
                            input_frontier: frontier.clone(),
                        };
                        (head, handle)
                    }
                };
                // Native progress advances even when content is unchanged and
                // the durable head retains its publication frontier.
                let ready = ReadyEnvironment {
                    head,
                    body: candidate,
                    handle,
                    frontier: frontier.clone(),
                };
                next = Some(ready.clone());
                ready
            }
        };
        let (round, body, handle) = (ready.head.round(), ready.body, ready.handle);
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

async fn hydrate(
    store: &dyn EnvironmentStore,
    session: &lash_kernel_value::identity::SessionId,
) -> Result<EnvironmentProjection, DurableError> {
    Ok(match store.environment_head(session).await? {
        Some(head) => {
            let body = load_body(store, &head.round()).await?;
            EnvironmentProjection::Hydrated { head, body }
        }
        None => EnvironmentProjection::Empty,
    })
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
    cx.commit(tx, CommitLabel::ENVIRONMENT_PREPARE).await?;
    let store = cx.backend().stores().package_store();
    let reused = if let Some(prepared) = handle.prepared_publications() {
        let claim = crate::ReferrerClaim::guarded(crate::ReferrerGuard::EnvironmentVersionRetired(
            owner.id().clone(),
        ));
        match store.acquire_prepared_publications(&claim, prepared).await {
            Ok(()) => true,
            Err(lash_durable::DurableError::Package(
                lash_durable::packages::PackageRefusal::Missing(_),
            )) => false,
            Err(error) => return Err(error.into()),
        }
    } else {
        false
    };
    if !reused {
        session
            .plugins()
            .composition()
            .retain_environment_packages(owner.id(), handle.surface().packages(), store.as_ref())
            .await
            .map_err(lash_execution_contracts::PluginError::from)?;
        let prepared = store
            .prepare_publications(&handle.surface().packages().publications())
            .await?;
        handle.remember_publications(prepared);
    }
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
                    incarnation: lash_core_store::environment::fresh_identity(),
                    counter: 0,
                }),
            fragment: EnvironmentFragment::default(),
        })
        .collect()
}
