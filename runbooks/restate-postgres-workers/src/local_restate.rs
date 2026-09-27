//! A one-process host's Restate deployment on a local `restate-server`: the
//! zero-infra effect engine (ADR 0104 §4) a companion binary runs against
//! when `scripts/ci/with-service.sh restate` hands it the server's addresses.
//!
//! The host builds `lash-restate`'s engine over its store set, serves the
//! engine's endpoint on a loopback port and registers that port with the
//! server's admin API. The server then drives every turn and process in the
//! endpoint's handlers; the host only sends (D5).

use std::sync::Arc;

use anyhow::{Context, Result};

/// The addresses `scripts/ci/with-service.sh restate` exports.
#[derive(Clone, Debug)]
pub struct LocalRestate {
    pub ingress_url: String,
    pub admin_url: String,
    pub authority: lash_restate::RestateAuthorityId,
}

impl LocalRestate {
    /// Read `RESTATE_INGRESS_URL`, `RESTATE_ADMIN_URL` and
    /// `RESTATE_AUTHORITY_ID`; a companion run outside the service wrapper
    /// is refused, naming the wrapper.
    pub fn from_env() -> Result<Self> {
        let read = |name: &str| {
            std::env::var(name).with_context(|| {
                format!("{name} is unset: run under `scripts/ci/with-service.sh restate --`")
            })
        };
        Ok(Self {
            ingress_url: read("RESTATE_INGRESS_URL")?,
            admin_url: read("RESTATE_ADMIN_URL")?,
            authority: lash_restate::RestateAuthorityId::new(read("RESTATE_AUTHORITY_ID")?)
                .map_err(|error| anyhow::anyhow!("RESTATE_AUTHORITY_ID: {error}"))?,
        })
    }

    /// The engine over `stores`, reaching this server.
    pub fn engine(&self, stores: Arc<dyn lash::StoreSet>) -> Arc<lash_restate::RestateEngine> {
        Arc::new(lash_restate::RestateEngine::new(
            stores,
            lash::restate::config(
                self.ingress_url.clone(),
                self.admin_url.clone(),
                self.authority.clone(),
            ),
        ))
    }

    /// Serve `endpoint` on a free loopback port and register it with the
    /// server. The deployment serves until the returned handle drops.
    pub async fn serve(
        &self,
        engine: &lash_restate::RestateEngine,
        endpoint: restate_sdk::endpoint::Endpoint,
    ) -> Result<LocalDeployment> {
        self.serve_at(
            engine,
            std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            endpoint,
        )
        .await
    }

    /// Serve `endpoint` at `addr` and register it with the server, which
    /// refuses it when another deployment holds the engine's namespace's
    /// names. The deployment serves until the returned handle drops.
    pub async fn serve_at(
        &self,
        engine: &lash_restate::RestateEngine,
        addr: std::net::SocketAddr,
        endpoint: restate_sdk::endpoint::Endpoint,
    ) -> Result<LocalDeployment> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind the Restate endpoint at {addr}"))?;
        let uri = format!("http://{}", listener.local_addr()?);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::spawn(async move {
            lash::restate::serve_endpoint(listener, endpoint, async move {
                let _ = stopped.await;
            })
            .await;
        });
        engine
            .register_deployment(&uri)
            .await
            .with_context(|| format!("register the Restate deployment at {uri}"))?;
        Ok(LocalDeployment {
            stop: Some(stop),
            serving,
        })
    }
}

/// A served, registered endpoint. Dropping it stops serving.
pub struct LocalDeployment {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    serving: tokio::task::JoinHandle<()>,
}

impl Drop for LocalDeployment {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.serving.abort();
    }
}
