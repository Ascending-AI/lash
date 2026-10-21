use lash::plugins::{PluginError, SessionGraphService};
use lash::plugins::{SessionGraphAppendResult, AppendSessionNodesRequest};

struct HostService;

#[lash::async_trait]
impl SessionGraphService for HostService {
    async fn append_session_nodes(
        &self,
        _: &lash_vocabulary::identity::SessionId,
        _: AppendSessionNodesRequest,
    ) -> Result<SessionGraphAppendResult, PluginError> {
        Err(PluginError::Session(String::new()))
    }
}

fn main() {}
