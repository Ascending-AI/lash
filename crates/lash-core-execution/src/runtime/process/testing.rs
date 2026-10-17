#![cfg(any(test, feature = "testing"))]

mod registry_faults;

pub use registry_faults::{
    NonTerminalPagePause, NonTerminalPageRead, ProcessRegistryFaults, RegistrationHoldPoint,
};
