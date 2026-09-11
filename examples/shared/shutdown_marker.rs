//! Opt-in shutdown witness for deterministic host lifecycle runbooks.
//!
//! The marker is written by the installed plugin factory's awaited shutdown
//! hook. Process disappearance alone is not evidence that factory teardown ran.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use lash::plugins::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin,
};

pub(crate) const MARKER_ENV: &str = "LASH_HOST_SHUTDOWN_MARKER";

pub(crate) fn factory_from_env(
    host: &'static str,
) -> Result<Option<Arc<dyn PluginFactory>>, String> {
    let path = match std::env::var(MARKER_ENV) {
        Ok(path) if !path.trim().is_empty() => PathBuf::from(path),
        Ok(_) => return Err(format!("{MARKER_ENV} must not be empty")),
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(error) => return Err(format!("read {MARKER_ENV}: {error}")),
    };
    Ok(Some(Arc::new(ShutdownMarkerFactory { host, path })))
}

struct ShutdownMarkerFactory {
    host: &'static str,
    path: PathBuf,
}

#[async_trait::async_trait]
impl PluginFactory for ShutdownMarkerFactory {
    fn id(&self) -> &'static str {
        "host_shutdown_marker"
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(ShutdownMarkerSession))
    }

    async fn shutdown(&self) -> Result<(), PluginError> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| PluginError::Session(format!("open shutdown marker: {error}")))?;
        writeln!(
            file,
            "host={} plugin_factory=host_shutdown_marker phase=shutdown_completed",
            self.host
        )
        .and_then(|_| file.sync_all())
        .map_err(|error| PluginError::Session(format!("persist shutdown marker: {error}")))
    }
}

struct ShutdownMarkerSession;

impl SessionPlugin for ShutdownMarkerSession {
    fn id(&self) -> &'static str {
        "host_shutdown_marker"
    }

    fn register(&self, _registrar: &mut PluginRegistrar) -> Result<(), PluginError> {
        Ok(())
    }
}
