#![cfg(any(test, feature = "testing"))]

#[cfg(test)]
mod identity;

mod registry_faults;

pub use registry_faults::{
    NonTerminalPagePause, NonTerminalPageRead, ProcessRegistryFaults, RegistrationHoldPoint,
};
