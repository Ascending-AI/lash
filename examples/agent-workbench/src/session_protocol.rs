//! The protocol the host composes at startup and states for each send.
use anyhow::{Result, bail};
#[derive(Clone, Copy)]
pub(crate) enum SessionProtocol {
    Standard,
    Rlm,
}
pub(crate) fn selected() -> Result<SessionProtocol> {
    match std::env::var("AGENT_WORKBENCH_PROTOCOL").as_deref() {
        Ok("standard") => Ok(SessionProtocol::Standard),
        Ok("rlm") | Err(std::env::VarError::NotPresent) => Ok(SessionProtocol::Rlm),
        Ok(value) => bail!("unknown AGENT_WORKBENCH_PROTOCOL {value}"),
        Err(error) => Err(anyhow::anyhow!("invalid AGENT_WORKBENCH_PROTOCOL: {error}")),
    }
}
