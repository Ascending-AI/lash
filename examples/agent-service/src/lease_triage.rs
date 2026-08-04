//! The operator triage surface for a chat whose turn appears stuck.
//!
//! This is the host half of the procedure documented at
//! `docs/operations.html#stuck-turn`. lash supplies one lever,
//! [`LashCore::session_lease_diagnostics`], a snapshot read of the session's
//! execution-lease row, and this example turns that raw reading into the
//! host-owned classification an operator actually wants, exactly the way the
//! process rail turns `ObservedProcess`'s raw lease facts into a host verdict.
//!
//! The reading is never authority. A lapsed lease does not mean the turn failed:
//! the displaced holder may still win the commit compare-and-set, which is why
//! `LeaseLost` below explicitly tells the operator not to kill anything and to
//! read the `session_execution_lease.*` trace timeline instead.

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use lash::LashCore;
use lash::persistence::{SessionLeaseDiagnostics, SessionLeaseRenewal};
use serde::Serialize;

use crate::state::{AppResult, AppStateData};

/// What the lease reading alone can tell an operator about a stuck turn.
///
/// Deliberately *not* a "stuck" verdict: each variant names a different next
/// step, and two of the three require corroborating evidence the lease row
/// cannot carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LeaseTriage {
    /// No durable session exists under this id: never created, or deleted.
    NoSession,
    /// The session exists and nobody holds its execution lane. A turn that is
    /// "running" from the host's point of view while the lane is unheld either
    /// already committed and released, or never claimed.
    Unheld,
    /// A holder's renewals were current at the observation. The lane is healthy,
    /// so a turn that is not progressing is blocked inside itself, most often in
    /// a provider call with no timeout. Look at the provider, not the lease.
    ProviderHangShape,
    /// The holder's renewals stopped. A peer may take the lane over with a higher
    /// generation, and `session_execution_lease.renew_failed` →
    /// `session_execution_lease.taken_over` orders the handoff in the log.
    ///
    /// **Do not kill the displaced runner on this reading.** It may still commit;
    /// only `session_execution_lease.commit_cas_rejected` proves it did not.
    LeaseLost,
}

/// The triage answer plus the raw facts it was derived from, so an operator can
/// disagree with the classification without re-reading the store.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct LeaseTriageReport {
    pub(crate) session_id: String,
    pub(crate) triage: LeaseTriage,
    /// What to do next, in one line. The lease row never authorizes a kill.
    pub(crate) next_step: &'static str,
    pub(crate) observed_at_epoch_ms: Option<u64>,
    pub(crate) holder_owner_id: Option<String>,
    pub(crate) holder_incarnation_id: Option<String>,
    /// The lane's fencing generation (ADR 0029). A takeover advances it.
    pub(crate) generation: Option<u64>,
    pub(crate) claimed_at_epoch_ms: Option<u64>,
    pub(crate) expires_at_epoch_ms: Option<u64>,
    /// Milliseconds of headroom left on the lease, when renewals were current.
    pub(crate) expires_in_ms: Option<u64>,
    /// Milliseconds since the lease lapsed, when renewals had stopped.
    pub(crate) expired_for_ms: Option<u64>,
}

impl LeaseTriage {
    fn next_step(self) -> &'static str {
        match self {
            Self::NoSession => {
                "no durable session under this id; check the host's own chat record before looking \
                 at lash"
            }
            Self::Unheld => {
                "nobody holds the lane; reconcile the host's in-flight record against the session's \
                 committed head"
            }
            Self::ProviderHangShape => {
                "the lane is healthy, so the turn is blocked inside itself; inspect the provider \
                 call and cancel the exact turn if it must stop"
            }
            Self::LeaseLost => {
                "renewals stopped; read session_execution_lease.renew_failed / .taken_over for the \
                 handoff and do not kill the displaced runner, which may still commit"
            }
        }
    }
}

impl LeaseTriageReport {
    /// Apply the documented procedure to one lease reading.
    ///
    /// `None` is the absent-session answer, which the facade reports distinctly
    /// from a session whose lane is merely unheld.
    pub(crate) fn classify(
        session_id: &str,
        diagnostics: Option<&SessionLeaseDiagnostics>,
    ) -> Self {
        let Some(diagnostics) = diagnostics else {
            return Self {
                session_id: session_id.to_string(),
                triage: LeaseTriage::NoSession,
                next_step: LeaseTriage::NoSession.next_step(),
                observed_at_epoch_ms: None,
                holder_owner_id: None,
                holder_incarnation_id: None,
                generation: None,
                claimed_at_epoch_ms: None,
                expires_at_epoch_ms: None,
                expires_in_ms: None,
                expired_for_ms: None,
            };
        };
        let renewal = diagnostics.renewal();
        let triage = match renewal {
            SessionLeaseRenewal::Unheld => LeaseTriage::Unheld,
            SessionLeaseRenewal::Current { .. } => LeaseTriage::ProviderHangShape,
            SessionLeaseRenewal::Lapsed { .. } => LeaseTriage::LeaseLost,
        };
        let holder = diagnostics.holder.as_ref();
        Self {
            session_id: diagnostics.session_id.clone(),
            triage,
            next_step: triage.next_step(),
            observed_at_epoch_ms: Some(diagnostics.observed_at_epoch_ms),
            holder_owner_id: holder.map(|holder| holder.owner.owner_id.clone()),
            holder_incarnation_id: holder.map(|holder| holder.owner.incarnation_id.clone()),
            generation: holder.map(|holder| holder.generation),
            claimed_at_epoch_ms: holder.map(|holder| holder.claimed_at_epoch_ms),
            expires_at_epoch_ms: holder.map(|holder| holder.expires_at_epoch_ms),
            expires_in_ms: match renewal {
                SessionLeaseRenewal::Current { expires_in_ms } => Some(expires_in_ms),
                _ => None,
            },
            expired_for_ms: match renewal {
                SessionLeaseRenewal::Lapsed { expired_for_ms } => Some(expired_for_ms),
                _ => None,
            },
        }
    }

    /// Read the lane and classify it in one step.
    pub(crate) async fn read(core: &LashCore, session_id: &str) -> lash::Result<Self> {
        let diagnostics = core.session_lease_diagnostics(session_id).await?;
        Ok(Self::classify(session_id, diagnostics.as_ref()))
    }
}

/// `GET /api/chats/{chat_id}/lease`, the operator read. Diagnostics only: this
/// endpoint deliberately exposes no lever that acts on the lease.
pub(crate) async fn chat_lease_triage(
    State(state): State<AppStateData>,
    AxumPath(chat_id): AxumPath<String>,
) -> AppResult<Json<LeaseTriageReport>> {
    Ok(Json(LeaseTriageReport::read(state.core(), &chat_id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash::persistence::{
        LeaseOwnerIdentity, SessionLeaseHolder, SessionStoreCreateRequest, SessionStoreFactory,
    };
    use lash::{LashCore, ModelSpec};
    use lash_sqlite_store::SqliteSessionStoreFactory;
    use std::sync::Arc;

    const SESSION_ID: &str = "lease-triage-session";

    fn owner(owner_id: &str, incarnation: &str) -> LeaseOwnerIdentity {
        LeaseOwnerIdentity::opaque(owner_id, incarnation)
    }

    fn store_request(session_id: &str) -> SessionStoreCreateRequest {
        SessionStoreCreateRequest {
            session_id: session_id.to_string(),
            relation: lash::persistence::SessionRelation::default(),
            policy: lash::runtime::SessionPolicy::default(),
        }
    }

    /// A durable core over a scratch SQLite root, with no provider: every test
    /// here reads and manipulates the lease lane directly, so no turn runs.
    async fn durable_core(dir: &std::path::Path) -> (LashCore, Arc<dyn SessionStoreFactory>) {
        let factory: Arc<dyn SessionStoreFactory> =
            Arc::new(SqliteSessionStoreFactory::new(dir.join("sessions")));
        let core = LashCore::standard_builder()
            .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
            .attachment_store(Arc::new(
                lash::persistence::InMemoryAttachmentStore::default(),
            ))
            .process_env_store(Arc::new(
                lash::persistence::InMemoryProcessExecutionEnvStore::default(),
            ))
            .model(
                ModelSpec::from_token_limits("mock/model", Default::default(), 8_000, None)
                    .expect("valid model metadata"),
            )
            .store_factory(Arc::clone(&factory))
            .build()
            .expect("build durable core");
        (core, factory)
    }

    /// Materialize the session's durable store so `open_existing_store` resolves
    /// it, then hand back the store the lease lane lives on.
    async fn materialized_store(
        factory: &Arc<dyn SessionStoreFactory>,
        session_id: &str,
    ) -> Arc<dyn lash::persistence::RuntimePersistence> {
        factory
            .create_store(&store_request(session_id))
            .await
            .expect("create the session's durable store")
    }

    #[tokio::test]
    async fn an_unknown_session_reads_as_no_session_not_as_an_unheld_lane() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let (core, _factory) = durable_core(dir.path()).await;

        let report = LeaseTriageReport::read(&core, "never-created")
            .await
            .expect("diagnostic read of an unknown session");
        assert_eq!(report.triage, LeaseTriage::NoSession);
        assert_eq!(report.generation, None);
        assert!(
            core.session_lease_diagnostics("never-created")
                .await
                .expect("facade read")
                .is_none(),
            "an absent session must be distinguishable from an unheld lane"
        );
        println!("observed-triage {}", serde_json::json!(report));
    }

    #[tokio::test]
    async fn a_healthy_holder_reads_as_the_provider_hang_shape() {
        // The provider-hang situation: a runner is parked inside its own turn
        // while its renewal loop keeps the lane alive. The lease says "healthy",
        // which is exactly the answer that redirects triage to the provider.
        let dir = tempfile::tempdir().expect("scratch dir");
        let (core, factory) = durable_core(dir.path()).await;
        let store = materialized_store(&factory, SESSION_ID).await;
        let held = store
            .try_claim_session_execution_lease(
                SESSION_ID,
                &owner("worker-a", "worker-a:boot-1"),
                60_000,
            )
            .await
            .expect("claim the lane")
            .acquired()
            .expect("an unheld lane is acquirable");

        let report = LeaseTriageReport::read(&core, SESSION_ID)
            .await
            .expect("diagnostic read of a held lane");
        assert_eq!(report.triage, LeaseTriage::ProviderHangShape);
        assert_eq!(report.holder_owner_id.as_deref(), Some("worker-a"));
        assert_eq!(
            report.holder_incarnation_id.as_deref(),
            Some("worker-a:boot-1")
        );
        assert_eq!(report.generation, Some(held.fencing_token));
        assert!(
            report.expires_in_ms.is_some_and(|remaining| remaining > 0),
            "a current lease must report positive headroom: {report:?}"
        );
        assert_eq!(report.expired_for_ms, None);

        // The facade projection must agree field-for-field with the durable row
        // the store reports, so an operator reading the endpoint and an operator
        // reading the store never see two different truths.
        let diagnostics: SessionLeaseDiagnostics = core
            .session_lease_diagnostics(SESSION_ID)
            .await
            .expect("facade diagnostic read")
            .expect("a materialized session reports a reading");
        let holder: &SessionLeaseHolder = diagnostics
            .holder
            .as_ref()
            .expect("a held lane reports a holder");
        let row = store
            .get_session_execution_lease(SESSION_ID)
            .await
            .expect("store-level diagnostic read")
            .expect("a held lane reports its row");
        assert_eq!(holder.owner, row.owner);
        assert_eq!(holder.generation, row.fencing_token);
        assert_eq!(holder.claimed_at_epoch_ms, row.claimed_at_epoch_ms);
        assert_eq!(holder.expires_at_epoch_ms, row.expires_at_epoch_ms);
        assert_eq!(diagnostics.session_id, SESSION_ID);
        assert!(diagnostics.observed_at_epoch_ms > 0);
        assert!(matches!(
            diagnostics.renewal(),
            SessionLeaseRenewal::Current { .. }
        ));
        println!("observed-triage {}", serde_json::json!(report));
    }

    #[tokio::test]
    async fn a_lapsed_holder_reads_as_lease_loss_and_refuses_to_authorize_a_kill() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let (core, factory) = durable_core(dir.path()).await;
        let store = materialized_store(&factory, SESSION_ID).await;
        // TTL 0: the lane is held by a named owner whose renewals have already
        // stopped. This is the ambiguous state the procedure must not force.
        let stalled = store
            .try_claim_session_execution_lease(SESSION_ID, &owner("worker-a", "worker-a:boot-1"), 0)
            .await
            .expect("claim an immediately lapsed lane")
            .acquired()
            .expect("lapsed lane acquired");

        let report = LeaseTriageReport::read(&core, SESSION_ID)
            .await
            .expect("diagnostic read of a lapsed lane");
        assert_eq!(report.triage, LeaseTriage::LeaseLost);
        assert_eq!(report.holder_owner_id.as_deref(), Some("worker-a"));
        assert_eq!(report.generation, Some(stalled.fencing_token));
        assert!(report.expired_for_ms.is_some());
        assert!(
            report.next_step.contains("do not kill"),
            "lease loss must not read as authorization to kill: {}",
            report.next_step
        );
        println!("observed-triage {}", serde_json::json!(report));
    }

    #[tokio::test]
    async fn a_takeover_advances_the_generation_the_operator_reads() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let (core, factory) = durable_core(dir.path()).await;
        let store = materialized_store(&factory, SESSION_ID).await;
        let displaced = store
            .try_claim_session_execution_lease(SESSION_ID, &owner("worker-a", "worker-a:boot-1"), 0)
            .await
            .expect("claim an immediately lapsed lane")
            .acquired()
            .expect("lapsed lane acquired");
        let before = LeaseTriageReport::read(&core, SESSION_ID)
            .await
            .expect("read before takeover");

        let successor = store
            .try_claim_session_execution_lease(
                SESSION_ID,
                &owner("worker-b", "worker-b:boot-1"),
                60_000,
            )
            .await
            .expect("peer claim of a lapsed lane")
            .acquired()
            .expect("a lapsed lane is claimable");
        let after = LeaseTriageReport::read(&core, SESSION_ID)
            .await
            .expect("read after takeover");

        assert_eq!(before.holder_owner_id.as_deref(), Some("worker-a"));
        assert_eq!(after.holder_owner_id.as_deref(), Some("worker-b"));
        assert_eq!(after.triage, LeaseTriage::ProviderHangShape);
        assert!(
            after.generation > before.generation,
            "the operator read must show the takeover as a higher generation: {before:?} -> {after:?}"
        );
        assert_eq!(before.generation, Some(displaced.fencing_token));
        assert_eq!(after.generation, Some(successor.fencing_token));
        println!("observed-triage {}", serde_json::json!(before));
        println!("observed-triage {}", serde_json::json!(after));
    }

    #[tokio::test]
    async fn releasing_the_lane_reads_as_unheld_rather_than_absent() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let (core, factory) = durable_core(dir.path()).await;
        let store = materialized_store(&factory, SESSION_ID).await;
        let held = store
            .try_claim_session_execution_lease(
                SESSION_ID,
                &owner("worker-a", "worker-a:boot-1"),
                60_000,
            )
            .await
            .expect("claim the lane")
            .acquired()
            .expect("an unheld lane is acquirable");
        store
            .release_session_execution_lease(&held.completion())
            .await
            .expect("release the lane the way a committing turn does");

        let report = LeaseTriageReport::read(&core, SESSION_ID)
            .await
            .expect("diagnostic read after release");
        assert_eq!(report.triage, LeaseTriage::Unheld);
        assert_eq!(report.generation, None);
        assert!(
            core.session_lease_diagnostics(SESSION_ID)
                .await
                .expect("facade read")
                .is_some(),
            "a released lane still belongs to a session that exists"
        );
        println!("observed-triage {}", serde_json::json!(report));
    }

    #[tokio::test]
    async fn the_diagnostic_read_does_not_disturb_the_holder_it_reports() {
        // Running triage against a live session must be free: the read never
        // claims, renews, or releases, so the holder's fence still works after.
        let dir = tempfile::tempdir().expect("scratch dir");
        let (core, factory) = durable_core(dir.path()).await;
        let store = materialized_store(&factory, SESSION_ID).await;
        let held = store
            .try_claim_session_execution_lease(
                SESSION_ID,
                &owner("worker-a", "worker-a:boot-1"),
                60_000,
            )
            .await
            .expect("claim the lane")
            .acquired()
            .expect("an unheld lane is acquirable");

        for _ in 0..3 {
            let report = LeaseTriageReport::read(&core, SESSION_ID)
                .await
                .expect("repeated diagnostic reads");
            assert_eq!(report.generation, Some(held.fencing_token));
        }
        let renewed = store
            .renew_session_execution_lease(&held.fence(), 60_000)
            .await
            .expect("the holder's fence survives repeated diagnostic reads");
        assert_eq!(renewed.fencing_token, held.fencing_token);
    }
}
