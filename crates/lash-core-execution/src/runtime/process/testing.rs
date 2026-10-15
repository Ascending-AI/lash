#![cfg(any(test, feature = "testing"))]

#[cfg(test)]
mod identity;
mod registration_refusals;
mod registry_faults;
pub use registration_refusals::{
    REFUSAL_FIXTURE_START_KEY as PROCESS_REFUSAL_FIXTURE_START_KEY, accepted_process_registration,
    refused_process_registrations,
};
pub use registry_faults::{
    NonTerminalPagePause, NonTerminalPageRead, ProcessRegistryFaults, RegistrationHoldPoint,
};
