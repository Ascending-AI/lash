//! [`ActorContext`]: the one effect context of the durable substrate
//! (ADR 0132 §1, §3; I0, FIG-5194).
//!
//! An actor's activation runs everything it does through one concrete
//! context: no engine trait, no controller trait, no default bodies. The
//! context's core ([`ActorContext::begin`], [`ActorContext::commit`], the
//! clock, the cancel token, the replay probe and the due times) is real.

pub mod activation_loop;
mod claim_cell;
pub mod claim_obligations;
mod core;
pub mod ingress;
pub mod journal;
pub mod model_receipt;
pub mod owner_state;
pub mod process;
#[cfg(feature = "testing")]
pub mod process_laws;
pub mod resolutions;
pub mod round;
pub mod route_selection;
pub mod turn;
pub mod vm;
mod wait_effects;
#[cfg(feature = "testing")]
pub mod wait_laws;
pub mod waits;

pub use core::ActorContext;

impl ActorContext {
    /// A host start as the host rails realize it under `scope`: the host's
    /// own key makes the start idempotent while its process is retained, and
    /// a keyless start is always new, keyed by the scope and its ordinal
    /// among the run's keyless starts, so a durable handler's replay re-issues
    /// the same key (ADR 0107).
    ///
    /// # Errors
    ///
    /// `start_key_family_refused` for a request that carries a key of a
    /// family lash derives for its own start paths: a host mints only host
    /// keys, so a host rail never adopts a tool intent's process.
    pub fn key_host_start(
        &self,
        request: lash_execution_contracts::process::ProcessStartRequest,
    ) -> Result<
        lash_execution_contracts::process::ProcessStartRequest,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        match request.start_key() {
            Some(start_key) if start_key.is_host_supplied() => Ok(request),
            Some(_) => Err(
                lash_execution_contracts::plugin_error::PluginError::Runtime(
                    lash_core_store::runtime_error::RuntimeError::new(
                        lash_core_store::runtime_error::RuntimeErrorCode::StartKeyFamilyRefused,
                        "a host start carries only a host start key; this key is of a family lash \
                 derives for its own start paths",
                    ),
                ),
            ),
            None => {
                let start_key = self.next_keyless_start_key();
                Ok(lash_execution_contracts::process::model::key_start_request(
                    request,
                    Some(start_key),
                ))
            }
        }
    }
}
